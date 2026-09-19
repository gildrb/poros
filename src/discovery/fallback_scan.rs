use std::collections::BTreeSet;
use std::time::Duration;

const INSPECT_TIMEOUT: Duration = Duration::from_secs(2);

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

pub fn loopback_listeners(pids: &[u32]) -> Vec<String> {
    if pids.is_empty() {
        return Vec::new();
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
    // lsof exits 1 when no sockets match; failure yields an empty result.
    let output = capture(&arguments).unwrap_or_default();
    arguments.clear();
    let mut addresses: BTreeSet<String> = BTreeSet::new();
    for line in output.lines() {
        let Some(address) = line.strip_prefix('n') else {
            continue;
        };
        if let Some((host, port)) = address.rsplit_once(':') {
            if is_loopback_host(host) && port.parse::<u16>().is_ok() {
                addresses.insert(address.to_string());
            }
        }
    }
    addresses.into_iter().collect()
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
        .map(|ip| ip.is_loopback())
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
