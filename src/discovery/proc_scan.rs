use super::{Listener, ProcessInfo};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

/// Extracts (pid, ppid, pgid) from /proc/<pid>/stat. comm may contain spaces
/// and parentheses, so parsing starts after the last closing ')'.
fn stat_fields(text: &str) -> Option<(i64, i64, i64)> {
    let close = text.rfind(')')?;
    let pid: i64 = text[..close].split_whitespace().next()?.parse().ok()?;
    let mut fields = text[close + 1..].split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgid = fields.next()?.parse().ok()?;
    Some((pid, ppid, pgid))
}

/// Every process owned by the spawned child: its process group plus all
/// ppid-descendants, which covers servers that detach into new sessions.
pub fn owned_processes(root_pid: u32) -> Vec<u32> {
    let root = root_pid as i64;
    let mut table: HashMap<i64, (i64, i64)> = HashMap::new();
    for pid in proc_pids() {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some((pid, ppid, pgid)) = stat_fields(&text) {
                table.insert(pid, (ppid, pgid));
            }
        }
    }
    let root_pgid = table.get(&root).map(|(_, pgid)| *pgid).unwrap_or(root);
    let mut owned: BTreeSet<i64> = BTreeSet::new();
    for (pid, (_, pgid)) in &table {
        if *pgid == root_pgid {
            owned.insert(*pid);
        }
    }
    let mut frontier: Vec<i64> = owned.iter().copied().collect();
    while let Some(pid) = frontier.pop() {
        for (candidate, (ppid, _)) in &table {
            if *ppid == pid && owned.insert(*candidate) {
                frontier.push(*candidate);
            }
        }
    }
    owned.into_iter().map(|pid| pid as u32).collect()
}

fn proc_pids() -> impl Iterator<Item = u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
}

/// Every visible process with its parent, group, name, and command line.
pub fn process_table() -> Vec<ProcessInfo> {
    let mut processes = Vec::new();
    for pid in proc_pids() {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, ppid, pgid)) = stat_fields(&stat) else {
            continue;
        };
        let (Ok(ppid), Ok(pgid)) = (u32::try_from(ppid), u32::try_from(pgid)) else {
            continue;
        };
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|text| text.trim_end().to_string())
            .unwrap_or_default();
        let command = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter(|part| !part.is_empty())
                    .map(String::from_utf8_lossy)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
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

/// Maps socket inodes to the pid holding them. /proc/<pid>/fd is readable
/// only for the caller's own processes (or all of them as root).
fn socket_owners(pids: impl Iterator<Item = u32>) -> HashMap<u64, u32> {
    let mut owners = HashMap::new();
    for pid in pids {
        let directory = PathBuf::from(format!("/proc/{pid}/fd"));
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(inode) = target
                .to_str()
                .and_then(|text| text.strip_prefix("socket:["))
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|inode| inode.parse::<u64>().ok())
            else {
                continue;
            };
            owners.entry(inode).or_insert(pid);
        }
    }
    owners
}

/// Every TCP socket in LISTEN state held by one of `owners`.
fn listening_sockets(owners: &HashMap<u64, u32>) -> Vec<Listener> {
    let mut listeners = Vec::new();
    if owners.is_empty() {
        return listeners;
    }
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let columns: Vec<&str> = line.split_whitespace().collect();
            // sl, local, rem, st, ..., inode, ...; 0A is LISTEN.
            if columns.len() < 10 || columns[3] != "0A" {
                continue;
            }
            let Some(pid) = columns[9]
                .parse::<u64>()
                .ok()
                .and_then(|inode| owners.get(&inode))
            else {
                continue;
            };
            let Some((address_hex, port_hex)) = columns[1].split_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            let Some(ip) = parse_hex_address(address_hex) else {
                continue;
            };
            listeners.push(Listener {
                pid: *pid,
                ip,
                port,
            });
        }
    }
    listeners
}

/// Current directory of each pid, where readable.
pub fn working_directories(pids: &[u32]) -> HashMap<u32, String> {
    pids.iter()
        .filter_map(|pid| {
            let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
            Some((*pid, path.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Cheap fingerprint of every LISTEN socket (address, uid, inode): two small
/// kernel tables, no per-process work. Equal fingerprints mean the full
/// scan would find the same listeners.
pub fn listen_signature() -> String {
    let mut signature = String::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let columns: Vec<&str> = line.split_whitespace().collect();
            if columns.len() >= 10 && columns[3] == "0A" {
                signature.push_str(columns[1]);
                signature.push(' ');
                signature.push_str(columns[9]);
                signature.push('\n');
            }
        }
    }
    signature
}

/// Every listening TCP socket held by a process this user can inspect.
pub fn all_listeners() -> Vec<Listener> {
    listening_sockets(&socket_owners(proc_pids()))
}

/// Listening loopback sockets held by the owned processes. Socket inodes are
/// read from /proc/<pid>/fd and matched against /proc/net/tcp{,6}.
pub fn loopback_listeners(pids: &[u32]) -> Vec<String> {
    let mut addresses: BTreeSet<String> = BTreeSet::new();
    for listener in listening_sockets(&socket_owners(pids.iter().copied())) {
        // A wildcard bind (0.0.0.0 / ::) also serves loopback clients;
        // many dev servers default to it.
        let host = if listener.ip.is_unspecified() {
            "127.0.0.1".to_string()
        } else if listener.ip.is_loopback() {
            format_host(&listener.ip)
        } else {
            continue;
        };
        addresses.insert(format!("{host}:{}", listener.port));
    }
    addresses.into_iter().collect()
}

/// /proc/net stores IPv4 as the little-endian view of the big-endian address,
/// and IPv6 as four little-endian dwords.
fn parse_hex_address(text: &str) -> Option<std::net::IpAddr> {
    let bytes = |chunk: &str| -> Option<Vec<u8>> {
        (0..chunk.len() / 2)
            .map(|index| u8::from_str_radix(&chunk[index * 2..index * 2 + 2], 16))
            .collect::<Result<_, _>>()
            .ok()
    };
    if text.len() == 8 {
        let raw = bytes(text)?;
        Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
            raw[3], raw[2], raw[1], raw[0],
        )))
    } else if text.len() == 32 {
        let raw = bytes(text)?;
        let mut octets = [0u8; 16];
        for (index, octet) in raw.chunks(4).enumerate() {
            for (offset, byte) in octet.iter().rev().enumerate() {
                octets[index * 4 + offset] = *byte;
            }
        }
        Some(std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets)))
    } else {
        None
    }
}

fn format_host(ip: &std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return mapped.to_string();
            }
            format!("[{v6}]")
        }
    }
}
