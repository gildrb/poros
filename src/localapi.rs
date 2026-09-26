//! Minimal client for tailscaled's LocalAPI over its Unix socket. Poros holds
//! the foreground Serve session itself instead of keeping a `tailscale serve`
//! process (a ~28 MB Go runtime) alive for the whole run.

use serde::Deserialize;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Where tailscaled listens: Linux distributions, then the open-source
/// macOS daemon. The macOS GUI app has no socket; Poros uses its CLI.
const SOCKETS: [&str; 3] = [
    "/run/tailscale/tailscaled.sock",
    "/var/run/tailscale/tailscaled.sock",
    "/var/run/tailscaled.socket",
];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const SET_ATTEMPTS: u64 = 4;
/// NotifyInitialState (carries the SessionID) | NotifyRateLimit (netmap
/// updates at most every 3 s) | NotifyPeerChanges (deltas, not full maps).
const WATCH_MASK: u32 = (1 << 1) | (1 << 8) | (1 << 12);

pub struct LocalApi {
    socket: PathBuf,
}

struct Response {
    status: u16,
    etag: Option<String>,
    body: Vec<u8>,
}

/// A foreground Serve route. tailscaled deletes it when this connection
/// closes, including when Poros is killed outright.
pub struct Session {
    stream: UnixStream,
}

#[derive(Deserialize)]
struct Notify {
    #[serde(rename = "SessionID", default)]
    session_id: String,
}

impl LocalApi {
    pub fn discover() -> Option<Self> {
        SOCKETS.iter().find_map(|path| {
            let is_socket = std::fs::metadata(path)
                .map(|metadata| metadata.file_type().is_socket())
                .unwrap_or(false);
            is_socket.then(|| Self {
                socket: PathBuf::from(path),
            })
        })
    }

    pub fn status(&self) -> Result<String, String> {
        let response = self.exchange("GET", "/localapi/v0/status?peers=false", None, None)?;
        expect_ok(&response, "status")?;
        String::from_utf8(response.body).map_err(|error| format!("parse status: {error}"))
    }

    /// The Serve config JSON and its ETag.
    pub fn serve_config(&self) -> Result<(String, String), String> {
        let response = self.exchange("GET", "/localapi/v0/serve-config", None, None)?;
        expect_ok(&response, "Serve config")?;
        let etag = response
            .etag
            .ok_or("read Serve config: tailscaled sent no ETag")?;
        let body = String::from_utf8(response.body)
            .map_err(|error| format!("parse Serve config: {error}"))?;
        Ok((body, etag))
    }

    /// Opens a watch session and adds `https_port` → `bridge_url` as its
    /// foreground route, retrying when another client changed the config.
    pub fn open_route(
        &self,
        https_port: u16,
        authority: &str,
        bridge_url: &str,
    ) -> Result<Session, String> {
        let (stream, session_id) = self.watch()?;
        for attempt in 1..=SET_ATTEMPTS {
            let (config, etag) = self.serve_config()?;
            if crate::tailscale::parse_serve_config(&config)?.uses_port(https_port) {
                return Err(format!(
                    "HTTPS port {https_port} is already configured in Tailscale Serve; choose another with --https"
                ));
            }
            let body =
                with_foreground_route(&config, &session_id, https_port, authority, bridge_url)?;
            let response = self.exchange(
                "POST",
                "/localapi/v0/serve-config",
                Some(&etag),
                Some(body.as_bytes()),
            )?;
            match response.status {
                200 => {
                    stream
                        .set_nonblocking(true)
                        .map_err(|error| format!("configure Serve session: {error}"))?;
                    return Ok(Session { stream });
                }
                // Another client changed the config between read and write.
                412 if attempt < SET_ATTEMPTS => {
                    std::thread::sleep(Duration::from_millis(100 * attempt));
                }
                _ => return Err(describe_failure(&response, "set Serve config")),
            }
        }
        Err("set Serve config: the config kept changing; try again".to_string())
    }

    fn connect(&self) -> Result<UnixStream, String> {
        let stream = UnixStream::connect(&self.socket).map_err(|error| {
            format!(
                "connect to tailscaled at {}: {error}",
                self.socket.display()
            )
        })?;
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(REQUEST_TIMEOUT)))
            .map_err(|error| format!("configure tailscaled socket: {error}"))?;
        Ok(stream)
    }

    fn exchange(
        &self,
        method: &str,
        path: &str,
        if_match: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Response, String> {
        let mut stream = self.connect()?;
        let mut request = Vec::with_capacity(256 + body.map_or(0, <[u8]>::len));
        write!(
            request,
            "{method} {path} HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n"
        )
        .and_then(|()| match if_match {
            Some(etag) => write!(request, "If-Match: {etag}\r\n"),
            None => Ok(()),
        })
        .and_then(|()| match body {
            Some(body) => write!(
                request,
                "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .map(|()| request.extend_from_slice(body)),
            None => request.write_all(b"\r\n"),
        })
        .map_err(|error| error.to_string())?;
        stream
            .write_all(&request)
            .map_err(|error| format!("{method} {path}: {error}"))?;
        drop(request);
        let mut raw = Vec::new();
        stream
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|error| format!("{method} {path}: {error}"))?;
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(format!("{method} {path}: response too large"));
        }
        parse_response(raw).map_err(|error| format!("{method} {path}: {error}"))
    }

    /// Starts an IPN bus watch and returns the stream plus its session ID.
    fn watch(&self) -> Result<(UnixStream, String), String> {
        let mut stream = self.connect()?;
        // Connection: close makes an error answer end the stream; a 200
        // watch stays open for as long as the session lives.
        write!(
            stream,
            "GET /localapi/v0/watch-ipn-bus?mask={WATCH_MASK} HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n"
        )
        .map_err(|error| format!("watch tailscaled: {error}"))?;
        let mut raw = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        let first_line = loop {
            if let Some(line) = first_notify(&raw)? {
                break line;
            }
            if raw.len() > MAX_RESPONSE_BYTES {
                return Err("watch tailscaled: response too large".to_string());
            }
            match stream.read(&mut chunk) {
                Ok(0) => {
                    let response = parse_response(raw)?;
                    return Err(describe_failure(&response, "watch tailscaled"));
                }
                Ok(read) => raw.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(format!("watch tailscaled: {error}")),
            }
        };
        let notify: Notify = serde_json::from_slice(&first_line)
            .map_err(|error| format!("parse tailscaled notification: {error}"))?;
        if notify.session_id.is_empty() {
            return Err("tailscaled sent no Serve session ID".to_string());
        }
        stream
            .set_read_timeout(None)
            .map_err(|error| format!("configure Serve session: {error}"))?;
        Ok((stream, notify.session_id))
    }
}

impl Session {
    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Discards pending notifications without parsing them. Returns false
    /// once tailscaled ended the session (restart, reset, or config change).
    pub fn drain(&mut self) -> bool {
        let mut buffer = [0u8; 4096];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
    }
}

/// Inserts this session's route into the config without touching any other
/// field: tailscaled decodes the body into its own struct, so the round trip
/// through a generic value keeps everything it sent.
fn with_foreground_route(
    config: &str,
    session_id: &str,
    https_port: u16,
    authority: &str,
    bridge_url: &str,
) -> Result<String, String> {
    let mut document: serde_json::Value =
        serde_json::from_str(config).map_err(|error| format!("parse Serve config: {error}"))?;
    if document.is_null() {
        document = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(root) = document.as_object_mut() else {
        return Err("parse Serve config: expected an object".to_string());
    };
    let foreground = root
        .entry("Foreground")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if foreground.is_null() {
        *foreground = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(foreground) = foreground.as_object_mut() else {
        return Err("parse Serve config: Foreground is not an object".to_string());
    };
    foreground.insert(
        session_id.to_string(),
        serde_json::json!({
            "TCP": { https_port.to_string(): { "HTTPS": true } },
            "Web": { authority: { "Handlers": { "/": { "Proxy": bridge_url } } } },
        }),
    );
    serde_json::to_string(&document).map_err(|error| format!("encode Serve config: {error}"))
}

fn expect_ok(response: &Response, label: &str) -> Result<(), String> {
    if response.status == 200 {
        Ok(())
    } else {
        Err(describe_failure(response, &format!("read {label}")))
    }
}

fn describe_failure(response: &Response, action: &str) -> String {
    let text = String::from_utf8_lossy(&response.body);
    let text = text.trim();
    if response.status == 403 {
        return format!(
            "{action}: tailscaled denied access ({text}); an administrator can allow it with: sudo tailscale set --operator=$USER"
        );
    }
    format!("{action}: tailscaled answered {} {text}", response.status)
}

fn head_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

struct Head {
    status: u16,
    chunked: bool,
    etag: Option<String>,
}

fn parse_head(head: &[u8]) -> Result<Head, String> {
    let text = std::str::from_utf8(head).map_err(|_| "invalid response head".to_string())?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or("invalid response status")?;
    let mut chunked = false;
    let mut etag = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        } else if name.eq_ignore_ascii_case("etag") {
            etag = Some(value.to_string());
        }
    }
    Ok(Head {
        status,
        chunked,
        etag,
    })
}

fn parse_response(mut raw: Vec<u8>) -> Result<Response, String> {
    let end = head_end(&raw).ok_or("truncated response")?;
    let head = parse_head(&raw[..end])?;
    let body = if head.chunked {
        let (decoded, _) = decode_chunks(&raw[end..])?;
        decoded
    } else {
        raw.drain(..end);
        raw
    };
    Ok(Response {
        status: head.status,
        etag: head.etag,
        body,
    })
}

/// Decodes complete chunks; returns the payload and whether the terminating
/// zero-size chunk was seen.
fn decode_chunks(mut data: &[u8]) -> Result<(Vec<u8>, bool), String> {
    let mut payload = Vec::with_capacity(data.len());
    loop {
        let Some(line_end) = data.windows(2).position(|window| window == b"\r\n") else {
            return Ok((payload, false));
        };
        let size_text = std::str::from_utf8(&data[..line_end]).map_err(|_| "invalid chunk size")?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| "invalid chunk size")?;
        if size == 0 {
            return Ok((payload, true));
        }
        let start = line_end + 2;
        let Some(stop) = start.checked_add(size) else {
            return Err("invalid chunk size".to_string());
        };
        if data.len() < stop + 2 {
            return Ok((payload, false));
        }
        payload.extend_from_slice(&data[start..stop]);
        data = &data[stop + 2..];
    }
}

/// The first newline-terminated JSON notification of a watch response, once
/// it has fully arrived. Non-200 answers are reported as errors by the caller
/// after the server closes the connection.
fn first_notify(raw: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let Some(end) = head_end(raw) else {
        return Ok(None);
    };
    let head = parse_head(&raw[..end])?;
    if head.status != 200 {
        return Ok(None);
    }
    let payload = if head.chunked {
        decode_chunks(&raw[end..])?.0
    } else {
        raw[end..].to_vec()
    };
    Ok(payload
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|newline| payload[..newline].to_vec()))
}
