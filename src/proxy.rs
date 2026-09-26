use crate::cli::TargetUrl;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_REQUEST_BODY_BYTES: u64 = 16 * 1024 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Back-off after accept errors such as EMFILE, so they cannot spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct RequestHead {
    method: String,
    upgrade: bool,
    has_body_length: bool,
    chunked: bool,
    headers: Vec<(String, String)>,
    header_bytes: Vec<u8>,
}

pub struct Bridge {
    listener: TcpListener,
    target: TargetUrl,
    authority: String,
    shutdown: Shutdown,
}

/// Stops a bridge whose accept loop is blocked in the kernel: the flag is
/// set, then one loopback connection wakes the accept call.
#[derive(Clone)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
    address: SocketAddr,
}

impl Shutdown {
    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.address, BACKEND_CONNECT_TIMEOUT);
    }

    fn requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

impl Bridge {
    /// Binds the loopback bridge listener. The bound port is returned with the
    /// bridge so the caller can hand the exact address to Tailscale Serve.
    pub fn bind(target: TargetUrl, authority: String) -> Result<(Self, u16), String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        Ok((
            Self {
                listener,
                target,
                authority,
                shutdown: Shutdown {
                    flag: Arc::new(AtomicBool::new(false)),
                    address,
                },
            },
            address.port(),
        ))
    }

    pub fn shutdown_handle(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Blocking accept loop; one thread per connection. Returns after
    /// `Shutdown::stop`.
    pub fn serve(&self) {
        loop {
            let accepted = self.listener.accept();
            if self.shutdown.requested() {
                return;
            }
            match accepted {
                Ok((stream, _address)) => {
                    let shared = Shared {
                        target: self.target.clone(),
                        authority: self.authority.clone(),
                    };
                    let _ = std::thread::Builder::new()
                        .name("poros-conn".to_string())
                        .spawn(move || handle_connection(stream, shared));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionAborted | ErrorKind::Interrupted
                    ) => {}
                Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
            }
        }
    }
}

#[derive(Clone)]
struct Shared {
    target: TargetUrl,
    authority: String,
}

fn handle_connection(mut stream: TcpStream, shared: Shared) {
    let _ = stream.set_read_timeout(Some(HEADER_READ_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let head = match read_request_head(&mut stream) {
        Ok(Some(head)) => head,
        Ok(None) => return,
        Err(error) => {
            let status = if error.contains("too large") {
                431
            } else {
                400
            };
            let _ = write_simple_response(&mut stream, status, "Bad Request");
            return;
        }
    };
    if !host_matches(&head, &shared.authority) {
        let _ = write_simple_response(&mut stream, 403, "Host is not this Poros route");
        return;
    }
    if let Some(origin) = header_value(&head.headers, "Origin") {
        if !origin_matches(&origin, &shared.authority) {
            let _ = write_simple_response(
                &mut stream,
                403,
                "Origin does not match this Tailscale node",
            );
            return;
        }
    }
    let backend = match connect_backend(&shared.target) {
        Ok(backend) => backend,
        Err(_) => {
            let _ = write_simple_response(&mut stream, 502, "Development server is not ready");
            return;
        }
    };
    if head.upgrade {
        relay_upgrade(stream, backend, head);
    } else {
        relay_http(stream, backend, head, shared);
    }
}

fn read_request_head(stream: &mut TcpStream) -> Result<Option<RequestHead>, String> {
    let mut header_bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if find_header_end(&header_bytes).is_some() {
            break;
        }
        if header_bytes.len() > MAX_HEADER_BYTES {
            return Err("request head too large".to_string());
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                if header_bytes.is_empty() {
                    return Ok(None);
                }
                return Err("connection closed mid-request".to_string());
            }
            Ok(read) => header_bytes.extend_from_slice(&chunk[..read]),
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                return Err("request header timeout".to_string())
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    let text = String::from_utf8_lossy(&header_bytes).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let _path = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty() || !version.starts_with("HTTP/") {
        return Err("malformed request line".to_string());
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let connection_tokens = header_value(&headers, "Connection").unwrap_or_default();
    let upgrade = header_value(&headers, "Upgrade").is_some()
        && connection_tokens
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    let content_length =
        header_value(&headers, "Content-Length").and_then(|value| value.trim().parse::<u64>().ok());
    let chunked = header_value(&headers, "Transfer-Encoding")
        .map(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        })
        .unwrap_or(false);
    Ok(Some(RequestHead {
        method,
        upgrade,
        has_body_length: content_length.is_some(),
        chunked,
        headers,
        header_bytes,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn host_matches(head: &RequestHead, authority: &str) -> bool {
    header_value(&head.headers, "Host")
        .map(|host| host.trim().eq_ignore_ascii_case(authority))
        .unwrap_or(false)
}

/// Only the node's own HTTPS origin passes: scheme must be https and the
/// authority must be this route, or the bare MagicDNS hostname (the
/// certificate also answers it without a port).
fn origin_matches(origin: &str, authority: &str) -> bool {
    let Some(rest) = origin.strip_prefix("https://") else {
        return false;
    };
    let authoritative = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('.');
    if authoritative.eq_ignore_ascii_case(authority) {
        return true;
    }
    // Bare hostname without a port is the same origin space.
    let Some((origin_host, origin_port)) = authoritative.rsplit_once(':') else {
        let expected_host = authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority);
        return !authoritative.is_empty() && authoritative.eq_ignore_ascii_case(expected_host);
    };
    // Any port other than this route's is a different origin.
    let Some((expected_host, expected_port)) = authority.rsplit_once(':') else {
        return false;
    };
    origin_port == expected_port && origin_host.eq_ignore_ascii_case(expected_host)
}

fn connect_backend(target: &TargetUrl) -> Result<TcpStream, String> {
    let address = format!("{}:{}", target.host, target.port);
    let stream = TcpStream::connect_timeout(&resolve(&address)?, BACKEND_CONNECT_TIMEOUT)
        .map_err(|error| error.to_string())?;
    stream
        .set_nodelay(true)
        .map_err(|error| error.to_string())?;
    Ok(stream)
}

fn resolve(address: &str) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
    address
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
        .next()
        .ok_or_else(|| format!("no address for {address}"))
}

/// WebSocket pass-through: after the Host/Origin checks, the client's exact
/// request bytes go to the backend and both directions relay until close.
fn relay_upgrade(client: TcpStream, mut backend: TcpStream, head: RequestHead) {
    if backend.write_all(&head.header_bytes).is_err() {
        return;
    }
    let mut client_half = match client.try_clone() {
        Ok(half) => half,
        Err(_) => return,
    };
    let mut backend_half = match backend.try_clone() {
        Ok(half) => half,
        Err(_) => return,
    };
    let client_to_backend = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_half, &mut backend_half);
        let _ = backend_half.shutdown(std::net::Shutdown::Write);
    });
    let mut backend_read = backend;
    let mut client_write = client;
    let _ = std::io::copy(&mut backend_read, &mut client_write);
    let _ = client_write.shutdown(std::net::Shutdown::Write);
    let _ = client_to_backend.join();
}

/// Plain HTTP: rewrite to HTTP/1.0 (no keep-alive, no chunked ambiguity),
/// forward with forwarded headers, relay the response until EOF.
fn relay_http(mut client: TcpStream, mut backend: TcpStream, head: RequestHead, shared: Shared) {
    if head.chunked && !head.has_body_length {
        let _ = write_simple_response(&mut client, 411, "Length Required");
        return;
    }
    let content_length = header_value(&head.headers, "Content-Length")
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BODY_BYTES {
        let _ = write_simple_response(&mut client, 413, "Payload Too Large");
        return;
    }
    let request = build_forwarded_request(&head, content_length, &shared);
    if backend.write_all(&request).is_err() {
        let _ = write_simple_response(&mut client, 502, "Development server is not ready");
        return;
    }
    // Bytes read past the header end belong to the request body.
    let body_start = find_header_end(&head.header_bytes)
        .map(|start| start + 4)
        .unwrap_or(head.header_bytes.len());
    let buffered_body = &head.header_bytes[body_start..];
    if !buffered_body.is_empty() && backend.write_all(buffered_body).is_err() {
        return;
    }
    let relayed = buffered_body.len() as u64;
    if content_length > relayed
        && relay_exact(&mut client, &mut backend, content_length - relayed).is_err()
    {
        return;
    }
    // Relay the response head and body; rewrite absolute Location headers.
    let mut response_head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        match backend.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => {
                response_head.extend_from_slice(&chunk[..read]);
                if let Some(end) = find_header_end(&response_head) {
                    break end + 4;
                }
                if response_head.len() > MAX_HEADER_BYTES {
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let rewritten = rewrite_response_head(&response_head[..head_end], &shared);
    if client.write_all(&rewritten).is_err() {
        return;
    }
    if client.write_all(&response_head[head_end..]).is_err() {
        return;
    }
    let _ = std::io::copy(&mut backend, &mut client);
    let _ = client.shutdown(std::net::Shutdown::Write);
}

fn relay_exact(client: &mut TcpStream, backend: &mut TcpStream, length: u64) -> Result<(), ()> {
    let mut remaining = length;
    let mut buffer = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = client.read(&mut buffer[..want]).map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        backend.write_all(&buffer[..read]).map_err(|_| ())?;
        remaining -= read as u64;
    }
    Ok(())
}

fn build_forwarded_request(head: &RequestHead, content_length: u64, shared: &Shared) -> Vec<u8> {
    let mut request = Vec::with_capacity(head.header_bytes.len() + 128);
    // The path is already validated by Tailscale Serve routing; forward verbatim.
    let text = String::from_utf8_lossy(&head.header_bytes).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    request.extend_from_slice(format!("{} {} HTTP/1.0\r\n", head.method, path).as_bytes());
    let target_host = format!("{}:{}", shared.target.host, shared.target.port);
    let mut hop_by_hop_skipped = false;
    for (name, value) in &head.headers {
        let lowered = name.to_ascii_lowercase();
        if matches!(
            lowered.as_str(),
            "host"
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "transfer-encoding"
                | "upgrade"
                | "te"
                | "trailer"
        ) {
            hop_by_hop_skipped = true;
            continue;
        }
        request.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    let _ = hop_by_hop_skipped;
    request.extend_from_slice(format!("Host: {target_host}\r\n").as_bytes());
    if content_length > 0 {
        request.extend_from_slice(format!("Content-Length: {content_length}\r\n").as_bytes());
    }
    request.extend_from_slice(b"X-Forwarded-Proto: https\r\n");
    request.extend_from_slice(format!("X-Forwarded-Host: {}\r\n", shared.authority).as_bytes());
    request.extend_from_slice(b"X-Forwarded-For: 127.0.0.1\r\n");
    request.extend_from_slice(b"Via: 1.0 poros\r\n");
    request.extend_from_slice(b"Connection: close\r\n\r\n");
    request
}

/// Rewrites an absolute Location pointing at the local target back to the
/// public authority so browser redirects land on the HTTPS site.
fn rewrite_response_head(head: &[u8], shared: &Shared) -> Vec<u8> {
    let text = String::from_utf8_lossy(head).into_owned();
    let target_host = format!("{}:{}", shared.target.host, shared.target.port);
    let mut output = Vec::with_capacity(head.len() + 64);
    for line in text.split("\r\n") {
        if line.is_empty() {
            output.extend_from_slice(b"\r\n");
            break;
        }
        let rewritten = if let Some(rest) = line
            .strip_prefix("Location:")
            .or_else(|| line.strip_prefix("location:"))
        {
            let location = rest.trim();
            if let Some(path) = location
                .strip_prefix("http://")
                .and_then(|rest| rest.strip_prefix(target_host.as_str()))
            {
                format!("Location: https://{}{}\r\n", shared.authority, path)
            } else {
                format!("{line}\r\n")
            }
        } else {
            format!("{line}\r\n")
        };
        output.extend_from_slice(rewritten.as_bytes());
    }
    output
}

fn write_simple_response(
    stream: &mut TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let body = format!("{message}\n");
    let response = format!(
		"HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	);
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head_with(headers: &[(&str, &str)]) -> RequestHead {
        RequestHead {
            method: "GET".to_string(),
            upgrade: false,
            has_body_length: false,
            chunked: false,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            header_bytes: Vec::new(),
        }
    }

    const AUTHORITY: &str = "server.tail1234.ts.net:9119";

    #[test]
    fn host_must_match_authority_exactly() {
        assert!(host_matches(&head_with(&[("Host", AUTHORITY)]), AUTHORITY));
        assert!(host_matches(
            &head_with(&[("Host", "SERVER.TAIL1234.TS.NET:9119")]),
            AUTHORITY
        ));
        assert!(!host_matches(&head_with(&[]), AUTHORITY));
        assert!(!host_matches(
            &head_with(&[("Host", "evil.example.com:9119")]),
            AUTHORITY
        ));
        assert!(!host_matches(
            &head_with(&[("Host", "server.tail1234.ts.net")]),
            AUTHORITY
        ));
    }

    #[test]
    fn origin_must_be_own_https_site() {
        assert!(origin_matches(
            "https://server.tail1234.ts.net:9119",
            AUTHORITY
        ));
        assert!(origin_matches(
            "https://server.tail1234.ts.net:9119/",
            AUTHORITY
        ));
        assert!(origin_matches("https://server.tail1234.ts.net", AUTHORITY));
        assert!(!origin_matches(
            "http://server.tail1234.ts.net:9119",
            AUTHORITY
        ));
        assert!(!origin_matches("https://evil.example.com:9119", AUTHORITY));
        assert!(!origin_matches(
            "https://server.tail1234.ts.net:9119.evil.com",
            AUTHORITY
        ));
        assert!(!origin_matches("null", AUTHORITY));
        assert!(!origin_matches("", AUTHORITY));
    }

    #[test]
    fn location_rewrite_targets_public_authority() {
        let shared = Shared {
            target: TargetUrl {
                host: "127.0.0.1".to_string(),
                port: 3000,
            },
            authority: AUTHORITY.to_string(),
        };
        let response =
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:3000/login\r\n\r\n".to_vec();
        let rewritten = String::from_utf8(rewrite_response_head(&response, &shared)).expect("utf8");
        assert!(rewritten.contains("Location: https://server.tail1234.ts.net:9119/login"));
        let keep = b"HTTP/1.1 302 Found\r\nLocation: http://example.com/x\r\n\r\n".to_vec();
        assert!(String::from_utf8(rewrite_response_head(&keep, &shared))
            .expect("utf8")
            .contains("http://example.com/x"));
    }

    #[test]
    fn forwarded_request_strips_hop_by_hop_headers() {
        let shared = Shared {
            target: TargetUrl {
                host: "127.0.0.1".to_string(),
                port: 3000,
            },
            authority: AUTHORITY.to_string(),
        };
        let mut header_bytes = b"POST /api HTTP/1.1\r\n".to_vec();
        for line in [
            "Host: server.tail1234.ts.net:9119",
            "Connection: keep-alive",
            "Upgrade: websocket",
            "X-Custom: yes",
        ] {
            header_bytes.extend_from_slice(format!("{line}\r\n").as_bytes());
        }
        header_bytes.extend_from_slice(b"\r\n");
        let head = RequestHead {
            method: "POST".to_string(),
            upgrade: false,
            has_body_length: true,
            chunked: false,
            headers: vec![
                ("Host".to_string(), AUTHORITY.to_string()),
                ("Connection".to_string(), "keep-alive".to_string()),
                ("Upgrade".to_string(), "websocket".to_string()),
                ("X-Custom".to_string(), "yes".to_string()),
            ],
            header_bytes,
        };
        let request = String::from_utf8(build_forwarded_request(&head, 5, &shared)).expect("utf8");
        assert!(request.starts_with("POST /api HTTP/1.0\r\n"));
        assert!(request.contains("Host: 127.0.0.1:3000\r\n"));
        assert!(request.contains("X-Custom: yes\r\n"));
        assert!(request.contains("Content-Length: 5\r\n"));
        assert!(request.contains("X-Forwarded-Proto: https\r\n"));
        assert!(!request.contains("keep-alive"));
        assert!(!request.contains("Upgrade"));
        assert!(request.ends_with("Connection: close\r\n\r\n"));
    }
}
