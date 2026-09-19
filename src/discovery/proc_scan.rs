use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

/// Extracts (pid, ppid, pgid) from /proc/<pid>/stat. comm may contain spaces
/// and parentheses, so parsing starts after the last closing ')'.
fn stat_fields(text: &str) -> Option<(i64, i64, i64)> {
    let close = text.rfind(')')?;
    let pid: i64 = text[..close].split_whitespace().next()?.parse().ok()?;
    let after = &text[close + 1..];
    let fields = after.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 3 {
        return None;
    }
    Some((pid, fields[1].parse().ok()?, fields[2].parse().ok()?))
}

/// Every process owned by the spawned child: its process group plus all
/// ppid-descendants, which covers servers that detach into new sessions.
pub fn owned_processes(root_pid: u32) -> Vec<u32> {
    let root = root_pid as i64;
    let mut table: HashMap<i64, (i64, i64)> = HashMap::new();
    for entry in proc_entries() {
        if let Ok(text) = std::fs::read_to_string(&entry) {
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

fn proc_entries() -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
                .unwrap_or(false)
        })
        .map(|entry| entry.path().join("stat"))
}

/// Listening loopback sockets held by the owned processes. Socket inodes are
/// read from /proc/<pid>/fd and matched against /proc/net/tcp{,6}.
pub fn loopback_listeners(pids: &[u32]) -> Vec<String> {
    let mut inodes: BTreeSet<String> = BTreeSet::new();
    for pid in pids {
        let directory = PathBuf::from(format!("/proc/{pid}/fd"));
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let text = target.to_string_lossy();
            let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
            else {
                continue;
            };
            inodes.insert(inode.to_string());
        }
    }
    if inodes.is_empty() {
        return Vec::new();
    }
    let mut addresses: BTreeSet<String> = BTreeSet::new();
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
            let Some((address_hex, port_hex)) = columns[1].split_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            let Some(ip) = parse_hex_address(address_hex) else {
                continue;
            };
            // A wildcard bind (0.0.0.0 / ::) also serves loopback clients;
            // many dev servers default to it.
            if !ip.is_loopback() && !ip.is_unspecified() {
                continue;
            }
            let inode = columns[9].to_string();
            if !inodes.contains(&inode) {
                continue;
            }
            let host = if ip.is_unspecified() {
                "127.0.0.1".to_string()
            } else {
                format_host(&ip)
            };
            addresses.insert(format!("{host}:{port}"));
        }
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
