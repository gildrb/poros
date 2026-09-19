use crate::cli::TargetUrl;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const TICK: Duration = Duration::from_millis(150);

/// Polls the child's owned process set for exactly one live loopback HTTP
/// listener. Returns Ok(None) while nothing is listening yet.
pub fn discover_target(root_pid: u32) -> Result<Option<TargetUrl>, String> {
    let pids = owned_processes(root_pid);
    if pids.is_empty() {
        return Ok(None);
    }
    let listeners = loopback_listeners(&pids);
    let candidates = probe_listeners(listeners)?;
    match candidates.len() {
        0 => Ok(None),
        1 => {
            let address = &candidates[0];
            let (host, port) = split_host_port(address)?;
            Ok(Some(TargetUrl {
                host,
                port: port as u16,
            }))
        }
        _ => Err(format!(
            "multiple child HTTP listeners found: {:?}; select one with --target",
            candidates
        )),
    }
}

/// Runs until the deadline, polling at 150 ms. Cancellation comes from
/// checking the deadline at the call site between probes.
pub fn wait_for_target<F>(
    root_pid: u32,
    deadline: Instant,
    mut should_stop: F,
) -> Result<Option<TargetUrl>, String>
where
    F: FnMut() -> bool,
{
    loop {
        match discover_target(root_pid) {
            Ok(Some(target)) => return Ok(Some(target)),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if should_stop() {
            return Ok(None);
        }
        std::thread::sleep(TICK.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub fn required_tools_missing() -> Option<&'static str> {
    tool_missing("ps")
}

#[cfg(not(target_os = "linux"))]
fn tool_missing(name: &str) -> Option<&'static str> {
    for directory in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
        let candidate = directory.join(name);
        if let Ok(metadata) = std::fs::metadata(&candidate) {
            if metadata.is_file() {
                return None;
            }
        }
    }
    Some("lsof")
}

#[cfg(target_os = "linux")]
fn tool_missing(_name: &str) -> Option<&'static str> {
    None
}

fn probe_listeners(addresses: Vec<String>) -> Result<Vec<String>, String> {
    let mut responding = Vec::new();
    for address in addresses {
        if probe(address.as_str()) {
            responding.push(address);
        }
    }
    responding.sort();
    Ok(responding)
}

fn probe(address: &str) -> bool {
    let stream = TcpStream::connect_timeout(&parse_address(address), PROBE_TIMEOUT);
    let Ok(mut stream) = stream else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let request = format!("GET / HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut first = [0u8; 16];
    stream
        .read(&mut first)
        .map(|read| read > 0)
        .unwrap_or(false)
}

fn parse_address(address: &str) -> std::net::SocketAddr {
    use std::net::ToSocketAddrs;
    address
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .unwrap_or_else(|| "127.0.0.1:0".parse().expect("valid fallback address"))
}

fn split_host_port(address: &str) -> Result<(String, u32), String> {
    if let Some(inner) = address.strip_prefix('[') {
        let Some(close) = inner.find(']') else {
            return Err(format!("invalid listener address {address:?}"));
        };
        let host = inner[..close].to_string();
        let port = inner[close + 1..]
            .strip_prefix(':')
            .ok_or_else(|| format!("invalid listener address {address:?}"))?;
        let port: u32 = port
            .parse()
            .map_err(|_| format!("invalid listener port {address:?}"))?;
        return Ok((host, port));
    }
    let Some(colon) = address.rfind(':') else {
        return Err(format!("invalid listener address {address:?}"));
    };
    let host = address[..colon].to_string();
    let port: u32 = address[colon + 1..]
        .parse()
        .map_err(|_| format!("invalid listener port {address:?}"))?;
    Ok((host, port))
}

#[cfg(target_os = "linux")]
pub use proc_scan::{loopback_listeners, owned_processes};

#[cfg(target_os = "linux")]
mod proc_scan;

#[cfg(not(target_os = "linux"))]
pub use fallback_scan::{loopback_listeners, owned_processes};

#[cfg(not(target_os = "linux"))]
mod fallback_scan;
