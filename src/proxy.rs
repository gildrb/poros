use crate::cli::TargetUrl;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_REQUEST_BODY_BYTES: u64 = 16 * 1024 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Back-off after accept errors such as EMFILE, so they cannot spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
/// Workers keep at most ~20 KiB of buffers on the stack and do no name
/// resolution, so a small stack replaces the 2 MiB default.
const WORKER_STACK: usize = 64 * 1024;
/// Workers left waiting in accept(2) after a burst; the rest exit.
const MAX_IDLE_WORKERS: usize = 2;
const RELAY_BUFFER: usize = 8 * 1024;
const HEAD_CHUNK: usize = 2048;

/// Read-only per-route settings.
struct Shared {
    backend: SocketAddr,
    /// `host:port` of the target, sent as Host and matched in Location.
    target_host: String,
    authority: String,
}

/// Loopback listener in front of the dev server.
pub struct Bridge {
    listener: TcpListener,
    shared: Shared,
}

/// Workers block in accept(2) on the shared listener: the kernel hands each
/// connection to one sleeping worker, and a worker serves connection after
/// connection, so steady traffic creates no threads and an idle bridge has
/// no wakeups. A worker that takes the last idle slot starts a replacement
/// first; after a burst, workers beyond MAX_IDLE_WORKERS exit.
struct Pool {
    listener: TcpListener,
    shared: Shared,
    /// Workers in, or about to enter, accept(2).
    idle: AtomicUsize,
}

impl Bridge {
    /// Binds the loopback bridge listener and resolves the target once. The
    /// bound port is returned so the caller can hand it to Tailscale Serve.
    pub fn bind(target: &TargetUrl, authority: String) -> Result<(Self, u16), String> {
        let target_host = format!("{}:{}", target.host, target.port);
        let backend = target_host
            .to_socket_addrs()
            .map_err(|error| format!("resolve {target_host}: {error}"))?
            .next()
            .ok_or_else(|| format!("no address for {target_host}"))?;
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        Ok((
            Self {
                listener,
                shared: Shared {
                    backend,
                    target_host,
                    authority,
                },
            },
            port,
        ))
    }

    /// Starts serving on worker threads for the rest of the process.
    pub fn start(self) -> Result<(), String> {
        let pool = Arc::new(Pool {
            listener: self.listener,
            shared: self.shared,
            idle: AtomicUsize::new(0),
        });
        spawn_worker(&pool).map_err(|error| format!("start bridge: {error}"))
    }
}

fn spawn_worker(pool: &Arc<Pool>) -> std::io::Result<()> {
    pool.idle.fetch_add(1, Ordering::SeqCst);
    let worker_pool = Arc::clone(pool);
    let spawned = std::thread::Builder::new()
        .stack_size(WORKER_STACK)
        .spawn(move || worker(&worker_pool));
    if spawned.is_err() {
        pool.idle.fetch_sub(1, Ordering::SeqCst);
    }
    spawned.map(drop)
}

fn worker(pool: &Arc<Pool>) {
    loop {
        let accepted = pool.listener.accept();
        if pool.idle.fetch_sub(1, Ordering::SeqCst) == 1 {
            // On failure this worker returns to accept(2) once done, so
            // connections are only delayed, never stranded.
            let _ = spawn_worker(pool);
        }
        match accepted {
            Ok((stream, _address)) => handle_connection(stream, &pool.shared),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionAborted | ErrorKind::Interrupted
                ) => {}
            Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
        }
        if pool.idle.fetch_add(1, Ordering::SeqCst) >= MAX_IDLE_WORKERS {
            pool.idle.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    }
}

/// A parsed request head. Headers stay in the received bytes and are
/// iterated on demand: nothing is copied into per-header strings.
struct RequestHead {
    /// The head plus any body bytes that arrived with it.
    bytes: Vec<u8>,
    /// Index just past the blank line that ends the head.
    body_start: usize,
    upgrade: bool,
    chunked: bool,
    content_length: Option<u64>,
}

enum HeadError {
    Closed,
    TooLarge,
    Invalid,
}

impl RequestHead {
    fn parse(bytes: Vec<u8>, body_start: usize) -> Result<Self, HeadError> {
        let mut head = Self {
            bytes,
            body_start,
            upgrade: false,
            chunked: false,
            content_length: None,
        };
        let mut parts = head.request_line().split(|byte| *byte == b' ');
        let method = parts.next().unwrap_or_default();
        let _target = parts.next();
        let version = parts.next().unwrap_or_default();
        if method.is_empty() || !version.starts_with(b"HTTP/") {
            return Err(HeadError::Invalid);
        }
        head.upgrade = head.header("Upgrade").is_some()
            && head
                .header("Connection")
                .is_some_and(|value| has_token(value, b"upgrade"));
        head.chunked = head
            .header("Transfer-Encoding")
            .is_some_and(|value| has_token(value, b"chunked"));
        head.content_length = head.header("Content-Length").and_then(|value| {
            std::str::from_utf8(value)
                .ok()
                .and_then(|text| text.parse().ok())
        });
        Ok(head)
    }

    fn lines(&self) -> impl Iterator<Item = &[u8]> {
        // body_start - 4 drops the final CRLF CRLF.
        self.bytes[..self.body_start - 4]
            .split(|byte| *byte == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
    }

    fn request_line(&self) -> &[u8] {
        self.lines().next().unwrap_or_default()
    }

    fn method(&self) -> &[u8] {
        self.request_line()
            .split(|byte| *byte == b' ')
            .next()
            .unwrap_or_default()
    }

    fn target(&self) -> &[u8] {
        self.request_line()
            .split(|byte| *byte == b' ')
            .nth(1)
            .unwrap_or(b"/")
    }

    /// (name, value) pairs with surrounding whitespace trimmed.
    fn headers(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.lines().skip(1).filter_map(|line| {
            let colon = line.iter().position(|byte| *byte == b':')?;
            Some((line[..colon].trim_ascii(), line[colon + 1..].trim_ascii()))
        })
    }

    fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers()
            .find(|(key, _)| key.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, value)| value)
    }

    fn buffered_body(&self) -> &[u8] {
        &self.bytes[self.body_start..]
    }
}

fn has_token(value: &[u8], token: &[u8]) -> bool {
    value
        .split(|byte| *byte == b',')
        .any(|part| part.trim_ascii().eq_ignore_ascii_case(token))
}

fn handle_connection(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(HEADER_READ_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let head = match read_request_head(&mut stream) {
        Ok(head) => head,
        Err(HeadError::Closed) => return,
        Err(HeadError::TooLarge) => {
            let _ = write_simple_response(&mut stream, 431, "Bad Request");
            return;
        }
        Err(HeadError::Invalid) => {
            let _ = write_simple_response(&mut stream, 400, "Bad Request");
            return;
        }
    };
    if !host_matches(&head, &shared.authority) {
        let _ = write_simple_response(&mut stream, 403, "Host is not this Poros route");
        return;
    }
    if let Some(origin) = head.header("Origin") {
        let allowed = std::str::from_utf8(origin)
            .is_ok_and(|origin| origin_matches(origin, &shared.authority));
        if !allowed {
            let _ = write_simple_response(
                &mut stream,
                403,
                "Origin does not match this Tailscale node",
            );
            return;
        }
    }
    let backend = match TcpStream::connect_timeout(&shared.backend, BACKEND_CONNECT_TIMEOUT) {
        Ok(backend) => backend,
        Err(_) => {
            let _ = write_simple_response(&mut stream, 502, "Development server is not ready");
            return;
        }
    };
    let _ = backend.set_nodelay(true);
    if head.upgrade {
        relay_upgrade(stream, backend, &head, shared);
    } else {
        relay_http(stream, backend, &head, shared);
    }
}

fn read_request_head(stream: &mut TcpStream) -> Result<RequestHead, HeadError> {
    let mut bytes = Vec::with_capacity(HEAD_CHUNK);
    let mut chunk = [0u8; HEAD_CHUNK];
    loop {
        let searched = bytes.len().saturating_sub(3);
        match stream.read(&mut chunk) {
            Ok(0) if bytes.is_empty() => return Err(HeadError::Closed),
            Ok(0) => return Err(HeadError::Invalid),
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(HeadError::Invalid),
        }
        if let Some(end) = find_header_end(&bytes[searched..]) {
            return RequestHead::parse(bytes, searched + end + 4);
        }
        if bytes.len() > MAX_HEADER_BYTES {
            return Err(HeadError::TooLarge);
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn host_matches(head: &RequestHead, authority: &str) -> bool {
    head.header("Host")
        .is_some_and(|host| host.eq_ignore_ascii_case(authority.as_bytes()))
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

/// One direction of a WebSocket relay: bytes read from one socket and not
/// yet written to the other.
struct Pipe {
    buffer: [u8; RELAY_BUFFER],
    start: usize,
    end: usize,
    eof: bool,
    done: bool,
}

/// WebSocket pass-through: after the Host/Origin checks, the request goes to
/// the backend with the local Host (dev servers such as Vite reject upgrades
/// for foreign hosts), then one thread relays both directions with poll(2)
/// until both sides close. Idle sockets cost no wakeups.
fn relay_upgrade(client: TcpStream, backend: TcpStream, head: &RequestHead, shared: &Shared) {
    let request = build_upgrade_request(head, shared);
    if (&backend).write_all(&request).is_err()
        || client.set_nonblocking(true).is_err()
        || backend.set_nonblocking(true).is_err()
    {
        return;
    }
    drop(request);
    let streams = [&client, &backend];
    let mut pipes = [0, 1].map(|_| Pipe {
        buffer: [0; RELAY_BUFFER],
        start: 0,
        end: 0,
        eof: false,
        done: false,
    });
    loop {
        // Pipe `side` reads streams[side] and writes streams[1 - side].
        let mut events = [0 as libc::c_short; 2];
        for (side, pipe) in pipes.iter().enumerate() {
            if pipe.done {
                continue;
            }
            if pipe.start < pipe.end {
                events[1 - side] |= libc::POLLOUT;
            } else if !pipe.eof {
                events[side] |= libc::POLLIN;
            }
        }
        if events == [0, 0] {
            return;
        }
        // A socket with nothing requested is left out, so a hang-up there
        // cannot wake this loop repeatedly.
        let mut fds = [0, 1].map(|side| libc::pollfd {
            fd: if events[side] == 0 {
                -1
            } else {
                streams[side].as_raw_fd()
            },
            events: events[side],
            revents: 0,
        });
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        for (side, pipe) in pipes.iter_mut().enumerate() {
            if pipe.done {
                continue;
            }
            let (source, sink) = (streams[side], streams[1 - side]);
            if pipe.start == pipe.end && !pipe.eof && fds[side].revents != 0 {
                match (&*source).read(&mut pipe.buffer) {
                    Ok(0) => pipe.eof = true,
                    Ok(read) => {
                        pipe.start = 0;
                        pipe.end = read;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::WouldBlock | ErrorKind::Interrupted
                        ) => {}
                    Err(_) => pipe.eof = true,
                }
            }
            while pipe.start < pipe.end {
                match (&*sink).write(&pipe.buffer[pipe.start..pipe.end]) {
                    Ok(0) => return,
                    Ok(written) => pipe.start += written,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
            if pipe.eof && pipe.start == pipe.end {
                let _ = sink.shutdown(std::net::Shutdown::Write);
                pipe.done = true;
            }
        }
    }
}

/// Plain HTTP: rewrite to HTTP/1.0 (no keep-alive, no chunked ambiguity),
/// forward with forwarded headers, relay the response until EOF.
fn relay_http(mut client: TcpStream, mut backend: TcpStream, head: &RequestHead, shared: &Shared) {
    if head.chunked && head.content_length.is_none() {
        let _ = write_simple_response(&mut client, 411, "Length Required");
        return;
    }
    let content_length = head.content_length.unwrap_or(0);
    if content_length > MAX_REQUEST_BODY_BYTES {
        let _ = write_simple_response(&mut client, 413, "Payload Too Large");
        return;
    }
    let request = build_forwarded_request(head, content_length, shared);
    if backend.write_all(&request).is_err() {
        let _ = write_simple_response(&mut client, 502, "Development server is not ready");
        return;
    }
    drop(request);
    let buffered_body = head.buffered_body();
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
    let mut response_head = Vec::with_capacity(HEAD_CHUNK);
    let mut chunk = [0u8; HEAD_CHUNK];
    let head_end = loop {
        let searched = response_head.len().saturating_sub(3);
        match backend.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => {
                response_head.extend_from_slice(&chunk[..read]);
                if let Some(end) = find_header_end(&response_head[searched..]) {
                    break searched + end + 4;
                }
                if response_head.len() > MAX_HEADER_BYTES {
                    return;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    };
    let rewritten = rewrite_response_head(&response_head[..head_end], shared);
    if client.write_all(&rewritten).is_err() {
        return;
    }
    if client.write_all(&response_head[head_end..]).is_err() {
        return;
    }
    drop(rewritten);
    drop(response_head);
    let _ = std::io::copy(&mut backend, &mut client);
    let _ = client.shutdown(std::net::Shutdown::Write);
}

fn relay_exact(client: &mut TcpStream, backend: &mut TcpStream, length: u64) -> Result<(), ()> {
    let mut remaining = length;
    let mut buffer = [0u8; RELAY_BUFFER];
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(buffer.len(), |left| left.min(buffer.len()));
        let read = match client.read(&mut buffer[..want]) {
            Ok(0) | Err(_) => return Err(()),
            Ok(read) => read,
        };
        backend.write_all(&buffer[..read]).map_err(|_| ())?;
        remaining -= read as u64;
    }
    Ok(())
}

/// Hop-by-hop headers plus the ones Poros writes itself.
fn is_replaced_header(name: &[u8]) -> bool {
    const NAMES: [&[u8]; 9] = [
        b"host",
        b"connection",
        b"keep-alive",
        b"proxy-connection",
        b"transfer-encoding",
        b"upgrade",
        b"te",
        b"trailer",
        b"content-length",
    ];
    NAMES.iter().any(|known| name.eq_ignore_ascii_case(known))
}

fn build_forwarded_request(head: &RequestHead, content_length: u64, shared: &Shared) -> Vec<u8> {
    let mut request = Vec::with_capacity(head.body_start + 192);
    // The path is already validated by Tailscale Serve routing; forward verbatim.
    request.extend_from_slice(head.method());
    request.push(b' ');
    request.extend_from_slice(head.target());
    request.extend_from_slice(b" HTTP/1.0\r\n");
    for (name, value) in head.headers() {
        if is_replaced_header(name) {
            continue;
        }
        request.extend_from_slice(name);
        request.extend_from_slice(b": ");
        request.extend_from_slice(value);
        request.extend_from_slice(b"\r\n");
    }
    // Writing into a Vec cannot fail.
    let _ = write!(request, "Host: {}\r\n", shared.target_host);
    if content_length > 0 {
        let _ = write!(request, "Content-Length: {content_length}\r\n");
    }
    let _ = write!(
        request,
        "X-Forwarded-Proto: https\r\nX-Forwarded-Host: {}\r\nX-Forwarded-For: 127.0.0.1\r\nVia: 1.0 poros\r\nConnection: close\r\n\r\n",
        shared.authority
    );
    request
}

/// The upgrade request as received, except for the local Host, forwarded
/// headers, and any bytes that arrived after the head. Connection and
/// Upgrade stay: they carry the handshake.
fn build_upgrade_request(head: &RequestHead, shared: &Shared) -> Vec<u8> {
    let mut request = Vec::with_capacity(head.bytes.len() + 160);
    request.extend_from_slice(head.request_line());
    request.extend_from_slice(b"\r\n");
    for (name, value) in head.headers() {
        if name.eq_ignore_ascii_case(b"host") {
            continue;
        }
        request.extend_from_slice(name);
        request.extend_from_slice(b": ");
        request.extend_from_slice(value);
        request.extend_from_slice(b"\r\n");
    }
    // Writing into a Vec cannot fail.
    let _ = write!(
        request,
        "Host: {}\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: {}\r\nX-Forwarded-For: 127.0.0.1\r\n\r\n",
        shared.target_host, shared.authority
    );
    request.extend_from_slice(head.buffered_body());
    request
}

/// Rewrites an absolute Location pointing at the local target back to the
/// public authority so browser redirects land on the HTTPS site.
fn rewrite_response_head(head: &[u8], shared: &Shared) -> Vec<u8> {
    let mut output = Vec::with_capacity(head.len() + 64);
    let local_prefix = [b"http://".as_slice(), shared.target_host.as_bytes()];
    for line in head.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            output.extend_from_slice(b"\r\n");
            break;
        }
        let rewritten = line
            .split_at_checked(9)
            .filter(|(name, _)| name.eq_ignore_ascii_case(b"location:"))
            .and_then(|(_, value)| {
                value
                    .trim_ascii()
                    .strip_prefix(local_prefix[0])?
                    .strip_prefix(local_prefix[1])
            });
        match rewritten {
            Some(path) => {
                output.extend_from_slice(b"Location: https://");
                output.extend_from_slice(shared.authority.as_bytes());
                output.extend_from_slice(path);
            }
            None => output.extend_from_slice(line),
        }
        output.extend_from_slice(b"\r\n");
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
    let mut response = Vec::with_capacity(160 + message.len());
    write!(
		response,
		"HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}\n",
		message.len() + 1
	)?;
    stream.write_all(&response)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHORITY: &str = "server.tail1234.ts.net:9119";

    fn head_from(lines: &[&str]) -> RequestHead {
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"\r\n");
        let end = bytes.len();
        match RequestHead::parse(bytes, end) {
            Ok(head) => head,
            Err(_) => panic!("valid request head"),
        }
    }

    fn get_with(headers: &[&str]) -> RequestHead {
        let mut lines = vec!["GET / HTTP/1.1"];
        lines.extend_from_slice(headers);
        head_from(&lines)
    }

    fn shared() -> Shared {
        Shared {
            backend: "127.0.0.1:3000".parse().expect("address"),
            target_host: "127.0.0.1:3000".to_string(),
            authority: AUTHORITY.to_string(),
        }
    }

    #[test]
    fn host_must_match_authority_exactly() {
        assert!(host_matches(
            &get_with(&["Host: server.tail1234.ts.net:9119"]),
            AUTHORITY
        ));
        assert!(host_matches(
            &get_with(&["Host: SERVER.TAIL1234.TS.NET:9119"]),
            AUTHORITY
        ));
        assert!(!host_matches(&get_with(&[]), AUTHORITY));
        assert!(!host_matches(
            &get_with(&["Host: evil.example.com:9119"]),
            AUTHORITY
        ));
        assert!(!host_matches(
            &get_with(&["Host: server.tail1234.ts.net"]),
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
        let shared = shared();
        let response =
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:3000/login\r\n\r\n".to_vec();
        let rewritten = String::from_utf8(rewrite_response_head(&response, &shared)).expect("utf8");
        assert!(rewritten.contains("Location: https://server.tail1234.ts.net:9119/login"));
        assert!(rewritten.ends_with("\r\n\r\n"));
        let keep = b"HTTP/1.1 302 Found\r\nLocation: http://example.com/x\r\n\r\n".to_vec();
        assert!(String::from_utf8(rewrite_response_head(&keep, &shared))
            .expect("utf8")
            .contains("http://example.com/x"));
    }

    #[test]
    fn forwarded_request_strips_hop_by_hop_headers() {
        let head = head_from(&[
            "POST /api HTTP/1.1",
            "Host: server.tail1234.ts.net:9119",
            "Connection: keep-alive",
            "Upgrade: websocket",
            "Content-Length: 5",
            "X-Custom: yes",
        ]);
        let request =
            String::from_utf8(build_forwarded_request(&head, 5, &shared())).expect("utf8");
        assert!(request.starts_with("POST /api HTTP/1.0\r\n"));
        assert!(request.contains("Host: 127.0.0.1:3000\r\n"));
        assert!(request.contains("X-Custom: yes\r\n"));
        assert_eq!(request.matches("Content-Length: 5\r\n").count(), 1);
        assert!(request.contains("X-Forwarded-Proto: https\r\n"));
        assert!(!request.contains("keep-alive"));
        assert!(!request.contains("Upgrade"));
        assert!(request.ends_with("Connection: close\r\n\r\n"));
    }
}
