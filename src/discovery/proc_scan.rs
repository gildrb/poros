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
fn listening_sockets(owners: &HashMap<u64, u32>) -> Result<Vec<Listener>, String> {
    if owners.is_empty() {
        return Ok(Vec::new());
    }
    Ok(listen_table()?
        .into_iter()
        .filter_map(|socket| {
            owners.get(&socket.inode).map(|pid| Listener {
                pid: *pid,
                ip: socket.ip,
                port: socket.port,
            })
        })
        .collect())
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

/// Cheap fingerprint of every LISTEN socket (address, port, inode): one
/// kernel query, no per-process work. Equal fingerprints mean the full scan
/// would find the same listeners.
pub fn listen_signature() -> Result<String, String> {
    use std::fmt::Write as _;
    let mut signature = String::new();
    for socket in listen_table()? {
        // Writing to a String cannot fail.
        let _ = writeln!(signature, "{} {} {}", socket.ip, socket.port, socket.inode);
    }
    Ok(signature)
}

/// Every listening TCP socket held by a process this user can inspect.
pub fn all_listeners() -> Result<Vec<Listener>, String> {
    listening_sockets(&socket_owners(proc_pids()))
}

/// Listening loopback sockets held by the owned processes. Socket inodes are
/// read from /proc/<pid>/fd and matched against the kernel's LISTEN table.
pub fn loopback_listeners(pids: &[u32]) -> Result<Vec<String>, String> {
    let mut addresses: BTreeSet<String> = BTreeSet::new();
    for listener in listening_sockets(&socket_owners(pids.iter().copied()))? {
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
    Ok(addresses.into_iter().collect())
}

struct ListenSocket {
    ip: std::net::IpAddr,
    port: u16,
    inode: u64,
}

// Kernel UAPI: linux/netlink.h, linux/sock_diag.h, linux/inet_diag.h.
const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const TCP_LISTEN: u32 = 10;
// Address families and protocol as the u8 fields of inet_diag_req_v2.
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const IPPROTO_TCP: u8 = 6;
const NLMSG_HEADER: usize = 16;
/// sizeof(struct inet_diag_req_v2) and sizeof(struct inet_diag_msg).
const DIAG_REQUEST: usize = 56;
const DIAG_MESSAGE: usize = 72;
const REQUEST_LENGTH: u32 = 72;

/// LISTEN sockets from NETLINK_SOCK_DIAG, the interface `ss` uses. The
/// kernel filters by state, so the cost does not grow with the number of
/// established or TIME_WAIT connections the way /proc/net/tcp does.
fn listen_table() -> Result<Vec<ListenSocket>, String> {
    let mut sockets = Vec::new();
    for family in [AF_INET, AF_INET6] {
        dump_listeners(family, &mut sockets)
            .map_err(|error| format!("list listening sockets: {error}"))?;
    }
    Ok(sockets)
}

fn dump_listeners(family: u8, sockets: &mut Vec<ListenSocket>) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut request = [0u8; NLMSG_HEADER + DIAG_REQUEST];
    request[0..4].copy_from_slice(&REQUEST_LENGTH.to_ne_bytes());
    request[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    request[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    request[8..12].copy_from_slice(&1u32.to_ne_bytes());
    // inet_diag_req_v2: family, protocol, ext, pad, states bitmask, sockid.
    request[16] = family;
    request[17] = IPPROTO_TCP;
    request[20..24].copy_from_slice(&(1u32 << TCP_LISTEN).to_ne_bytes());
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            request.as_ptr().cast(),
            request.len(),
            0,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The kernel sizes each dump datagram to the reader's buffer.
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let received = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        let Ok(received) = usize::try_from(received) else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        };
        let mut offset = 0;
        while offset + NLMSG_HEADER <= received {
            let header = &buffer[offset..offset + NLMSG_HEADER];
            let length = u32::from_ne_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let kind = u16::from_ne_bytes([header[4], header[5]]);
            if length < NLMSG_HEADER || offset + length > received {
                return Err(std::io::Error::other("malformed netlink message"));
            }
            let payload = &buffer[offset + NLMSG_HEADER..offset + length];
            match kind {
                NLMSG_DONE => return Ok(()),
                NLMSG_ERROR => {
                    let code = payload
                        .get(0..4)
                        .map(|bytes| i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                        .unwrap_or(-libc::EIO);
                    return Err(std::io::Error::from_raw_os_error(-code));
                }
                SOCK_DIAG_BY_FAMILY if payload.len() >= DIAG_MESSAGE => {
                    if let Some(socket) = parse_diag_message(payload) {
                        sockets.push(socket);
                    }
                }
                _ => {}
            }
            // Messages are 4-byte aligned.
            offset += (length + 3) & !3;
        }
    }
}

/// struct inet_diag_msg: family u8, state u8, timer u8, retrans u8, then
/// inet_diag_sockid (sport be16 @4, dport be16 @6, src [u8;16] @8, ...),
/// expires, rqueue, wqueue, uid, and inode u32 @68.
fn parse_diag_message(message: &[u8]) -> Option<ListenSocket> {
    let port = u16::from_be_bytes([message[4], message[5]]);
    let source: [u8; 16] = message[8..24].try_into().ok()?;
    let ip = match message[0] {
        AF_INET => std::net::IpAddr::V4(std::net::Ipv4Addr::new(
            source[0], source[1], source[2], source[3],
        )),
        AF_INET6 => std::net::IpAddr::V6(std::net::Ipv6Addr::from(source)),
        _ => return None,
    };
    let inode = u32::from_ne_bytes(message[68..72].try_into().ok()?);
    Some(ListenSocket {
        ip,
        port,
        inode: u64::from(inode),
    })
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
