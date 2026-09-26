use super::{Listener, ProcessInfo};
use std::collections::BTreeSet;
use std::time::Duration;

const INSPECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Every visible process with its parent, group, name, and command line.
pub fn process_table() -> Vec<ProcessInfo> {
    let Ok(output) = capture(&["ps", "-axo", "pid=,ppid=,pgid=,command="]) else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(pgid)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(pgid)) = (pid.parse(), ppid.parse(), pgid.parse()) else {
            continue;
        };
        let command = fields.collect::<Vec<_>>().join(" ");
        let name = command
            .split_whitespace()
            .next()
            .and_then(|program| program.rsplit('/').next())
            .unwrap_or_default()
            .to_string();
        processes.push(ProcessInfo {
            pid,
            ppid,
            pgid,
            name,
            command,
        });
    }
    processes
}

/// Every listening TCP socket held by a process this user can inspect.
pub fn all_listeners() -> Result<Vec<Listener>, String> {
    // lsof exits 1 when no sockets match; `capture` keeps its output anyway.
    let output = capture(&["lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"])?;
    let mut listeners = Vec::new();
    let mut pid: Option<u32> = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse().ok();
            continue;
        }
        let (Some(address), Some(pid)) = (line.strip_prefix('n'), pid) else {
            continue;
        };
        let Some((host, port)) = address.rsplit_once(':') else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        let host = host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(host);
        let ip = if host == "*" {
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        } else if let Ok(ip) = host.parse() {
            ip
        } else {
            continue;
        };
        let listener = Listener { pid, ip, port };
        if !listeners.contains(&listener) {
            listeners.push(listener);
        }
    }
    Ok(listeners)
}

/// Current directory of each pid, where readable.
pub fn working_directories(pids: &[u32]) -> std::collections::HashMap<u32, String> {
    let mut directories = std::collections::HashMap::new();
    if pids.is_empty() {
        return directories;
    }
    let pid_list = pids
        .iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let output =
        capture(&["lsof", "-a", "-d", "cwd", "-p", pid_list.as_str(), "-Fpn"]).unwrap_or_default();
    let mut pid: Option<u32> = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse().ok();
        } else if let (Some(path), Some(pid)) = (line.strip_prefix('n'), pid) {
            directories.insert(pid, path.to_string());
        }
    }
    directories
}

/// Cheap fingerprint of every LISTEN socket: one `netstat` call instead of
/// `lsof` and `ps`. Equal fingerprints mean the full scan would find the same
/// listeners, barring a restart on the same address between two calls.
pub fn listen_signature() -> Result<String, String> {
    let output = capture(&["netstat", "-an", "-p", "tcp"])?;
    Ok(output
        .lines()
        .filter(|line| line.ends_with("LISTEN"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Non-Linux fallback: `ps` for process ownership and `lsof` for sockets,
/// mirroring the original Go implementation's tooling contract.
pub fn owned_processes(root_pid: u32) -> Vec<u32> {
    let output = match capture(&["ps", "-axo", "pid=,ppid=,pgid="]) {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    let mut owned: BTreeSet<u32> = BTreeSet::new();
    for line in output.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 3 {
            continue;
        }
        let (Ok(pid), Ok(pgid)) = (fields[0].parse::<u32>(), fields[2].parse::<u32>()) else {
            continue;
        };
        if pgid == root_pid {
            owned.insert(pid);
        }
    }
    owned.into_iter().collect()
}

pub fn loopback_listeners(pids: &[u32]) -> Result<Vec<String>, String> {
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let pid_list = pids
        .iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut arguments = vec![
        "lsof",
        "-nP",
        "-a",
        "-p",
        pid_list.as_str(),
        "-iTCP",
        "-sTCP:LISTEN",
        "-Fn",
    ];
    // lsof exits 1 when no sockets match; `capture` keeps its output anyway.
    let output = capture(&arguments)?;
    arguments.clear();
    let mut addresses: BTreeSet<String> = BTreeSet::new();
    for line in output.lines() {
        let Some(address) = line.strip_prefix('n') else {
            continue;
        };
        if let Some((host, port)) = address.rsplit_once(':') {
            if port.parse::<u16>().is_err() {
                continue;
            }
            // Wildcard binds ("*:port") serve loopback clients too; lsof also
            // reports them for servers bound to 0.0.0.0 or ::.
            let normalized = if host == "*" || is_loopback_host(host) {
                if let Some(plain) = host
                    .strip_prefix('[')
                    .and_then(|rest| rest.strip_suffix(']'))
                {
                    format!("[{plain}]:{port}")
                } else {
                    format!("127.0.0.1:{port}")
                }
            } else {
                continue;
            };
            addresses.insert(normalized);
        }
    }
    Ok(addresses.into_iter().collect())
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    if host == "localhost" {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback() || ip.is_unspecified())
        .unwrap_or(false)
}

fn capture(args: &[&str]) -> Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut stream = stdout;
        let _ = stream.read_to_end(&mut buffer);
        buffer
    });
    let deadline = std::time::Instant::now() + INSPECT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err("inspection timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let bytes = reader.join().map_err(|_| "reader panicked".to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
