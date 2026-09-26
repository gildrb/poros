use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const SERVE_STATUS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct TailnetNode {
    pub dns_name: String,
}

#[derive(Deserialize)]
struct StatusDocument {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Self", default)]
    self_status: Option<SelfStatus>,
}

#[derive(Deserialize)]
struct SelfStatus {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServeConfig {
    #[serde(rename = "TCP", default)]
    pub tcp: HashMap<String, serde_json::Value>,
    #[serde(rename = "Web", default)]
    pub web: HashMap<String, WebRoute>,
    #[serde(rename = "Foreground", default)]
    pub foreground: HashMap<String, Option<ServeConfig>>,
    #[serde(rename = "AllowFunnel", default)]
    pub allow_funnel: HashMap<String, bool>,
    #[serde(rename = "Services", default)]
    pub services: Option<serde_json::Value>,
    #[serde(rename = "ETag", default)]
    pub etag: Option<String>,
}

#[derive(Deserialize)]
pub struct WebRoute {
    #[serde(rename = "Handlers", default)]
    pub handlers: HashMap<String, WebHandler>,
}

#[derive(Deserialize)]
pub struct WebHandler {
    #[serde(rename = "Proxy", default)]
    pub proxy: String,
}

pub fn find_cli(explicit: Option<&str>) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = explicit {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(value) = std::env::var("TAILSCALE_CLI") {
        if !value.is_empty() {
            candidates.push(PathBuf::from(value));
        }
    }
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        ));
    }
    for candidate in candidates {
        if let Ok(metadata) = std::fs::metadata(&candidate) {
            if metadata.is_file() && is_executable(&metadata) {
                return Ok(candidate);
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("tailscale");
            if let Ok(metadata) = std::fs::metadata(&candidate) {
                if metadata.is_file() && is_executable(&metadata) {
                    return Ok(candidate);
                }
            }
        }
    }
    Err("Tailscale CLI not found; install Tailscale or set TAILSCALE_CLI".to_string())
}

fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

pub fn load_node(cli: &Path) -> Result<TailnetNode, String> {
    let output = run_capture(
        cli,
        ["status", "--json", "--peers=false"],
        STATUS_TIMEOUT,
        "status",
    )?;
    parse_node(&output)
}

pub fn parse_node(data: &str) -> Result<TailnetNode, String> {
    let document: StatusDocument =
        serde_json::from_str(data).map_err(|error| format!("parse Tailscale status: {error}"))?;
    if document.backend_state != "Running" {
        return Err(format!(
            "Tailscale is not running (state: {})",
            document.backend_state
        ));
    }
    let Some(self_status) = document.self_status else {
        return Err("this node has no Tailscale IP".to_string());
    };
    if self_status.dns_name.is_empty() {
        return Err("Tailscale MagicDNS and HTTPS certificates are required".to_string());
    }
    Ok(TailnetNode {
        dns_name: self_status.dns_name.trim_end_matches('.').to_string(),
    })
}

pub fn read_serve_config(cli: &Path) -> Result<ServeConfig, String> {
    let output = run_capture(
        cli,
        ["serve", "status", "--json"],
        SERVE_STATUS_TIMEOUT,
        "Serve status",
    )?;
    parse_serve_config(&output)
}

pub fn parse_serve_config(output: &str) -> Result<ServeConfig, String> {
    // Empty Serve state is reported as JSON null by some Tailscale versions.
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(ServeConfig::default());
    }
    serde_json::from_str(trimmed).map_err(|error| format!("unsupported Serve status: {error}"))
}

struct Captured {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_capture<const N: usize>(
    cli: &Path,
    args: [&str; N],
    timeout: Duration,
    label: &str,
) -> Result<String, String> {
    let mut child = Command::new(cli)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("read {label}: {error}"))?;
    let Some(captured) = wait_with_timeout(&mut child, timeout)? else {
        return Err(format!("read {label}: timed out"));
    };
    if !captured.status.success() {
        return Err(format!(
            "read {label}: {}",
            String::from_utf8_lossy(&captured.stderr).trim_end()
        ));
    }
    String::from_utf8(captured.stdout).map_err(|error| format!("parse {label}: {error}"))
}

fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<Option<Captured>, String> {
    use std::io::Read;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut stream) = child.stdout.take() {
                    let _ = stream.read_to_end(&mut stdout);
                }
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_end(&mut stderr);
                }
                return Ok(Some(Captured {
                    status,
                    stdout,
                    stderr,
                }));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("wait for Tailscale CLI: {error}")),
        }
    }
}

impl ServeConfig {
    pub fn uses_port(&self, port: u16) -> bool {
        let port_text = port.to_string();
        if self.tcp.contains_key(&port_text) {
            return true;
        }
        for host in self.web.keys() {
            if port_of(host) == Some(port) {
                return true;
            }
        }
        for (host, enabled) in &self.allow_funnel {
            if *enabled && port_of(host) == Some(port) {
                return true;
            }
        }
        for child in self.foreground.values().flatten() {
            if child.uses_port(port) {
                return true;
            }
        }
        false
    }

    /// The bridge owns the route when a foreground Serve session proxies the
    /// exact host to the exact bridge address without funnel exposure.
    pub fn owns_route(&self, authority: &str, bridge: &str) -> bool {
        for child in self.foreground.values() {
            let Some(child) = child else { continue };
            let Some(route) = child.web.get(authority) else {
                continue;
            };
            let Some(handler) = route.handlers.get("/") else {
                continue;
            };
            if handler.proxy == bridge && !child.allow_funnel.contains_key(authority) {
                return true;
            }
        }
        false
    }

    /// Every HTTPS URL that proxies to a loopback port, including foreground
    /// sessions, keyed by that local port.
    pub fn loopback_routes(&self) -> Vec<(u16, String)> {
        let mut routes = Vec::new();
        for (authority, route) in &self.web {
            let origin = match authority.strip_suffix(":443") {
                Some(host) => format!("https://{host}"),
                None => format!("https://{authority}"),
            };
            for (path, handler) in &route.handlers {
                if let Some(port) = loopback_proxy_port(&handler.proxy) {
                    let suffix = if path == "/" { "" } else { path.as_str() };
                    routes.push((port, format!("{origin}{suffix}")));
                }
            }
        }
        for child in self.foreground.values().flatten() {
            routes.extend(child.loopback_routes());
        }
        routes.sort();
        routes
    }
}

/// Port of a Serve proxy target on this machine's loopback, e.g.
/// `http://127.0.0.1:3000`, `localhost:3000`, or a bare `3000`.
fn loopback_proxy_port(proxy: &str) -> Option<u16> {
    let rest = proxy
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(proxy);
    let authority = rest.split('/').next().unwrap_or_default();
    let Some((host, port)) = authority.rsplit_once(':') else {
        return authority.parse().ok();
    };
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if loopback {
        port.parse().ok()
    } else {
        None
    }
}

fn port_of(host: &str) -> Option<u16> {
    let text = host.rsplit(':').next()?;
    text.parse().ok()
}
