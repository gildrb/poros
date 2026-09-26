//! `poros dashboard`: every listening TCP server this user can see, with its
//! Tailscale HTTPS URL when one is served, and a key to stop it.

use crate::discovery::{self, Listener, ProcessInfo};
use crate::signal::Events;
use crate::tailscale;
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Safety net for changes the listener fingerprint cannot see.
const FULL_SCAN_INTERVAL: Duration = Duration::from_secs(30);
/// Re-scan shortly after a stop so the row disappears promptly.
const AFTER_STOP_REFRESH: Duration = Duration::from_millis(400);

/// One stoppable unit: a Poros session (with its dev server's ports) or a
/// standalone listening process.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    pid: u32,
    /// Signal the whole process group: set only when the listener leads
    /// its own group, so a parent shell is never hit.
    group: bool,
    /// A process that Tailscale Serve proxies to and whose descendants
    /// listen, i.e. a Poros session. It stops its own tree on SIGTERM.
    session: bool,
    command: String,
    /// Working directory, `~`-relative; empty until `State::refresh` fills it.
    dir: String,
    endpoints: Vec<String>,
    urls: Vec<String>,
    first_port: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    Stop,
    Kill,
}

struct State {
    rows: Vec<Row>,
    selected: usize,
    offset: usize,
    routes: Vec<(u16, String)>,
    routes_key: BTreeSet<(u32, u16)>,
    tailscale_note: Option<String>,
    message: String,
    pending: Option<(u32, Pending)>,
    stopped: BTreeSet<u32>,
    own_pid: u32,
    signature: String,
    scanned_at: Option<Instant>,
    force_scan: bool,
}

pub fn run() -> i32 {
    let mut stdout = std::io::stdout();
    let mut state = State {
        rows: Vec::new(),
        selected: 0,
        offset: 0,
        routes: Vec::new(),
        routes_key: BTreeSet::new(),
        tailscale_note: None,
        message: String::new(),
        pending: None,
        stopped: BTreeSet::new(),
        own_pid: std::process::id(),
        signature: String::new(),
        scanned_at: None,
        force_scan: true,
    };
    state.refresh();
    if !terminal::is_tty() {
        return match print_table(&mut stdout, &state) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("poros: {error}");
                1
            }
        };
    }
    let events = match Events::install(&[libc::SIGWINCH]) {
        Ok(events) => events,
        Err(error) => {
            eprintln!("poros: {error}");
            return 1;
        }
    };
    let terminal = match terminal::Raw::enter() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("poros: {error}");
            return 1;
        }
    };
    let outcome = interact(&events, &mut state, &mut stdout);
    drop(terminal);
    match outcome {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("poros: {error}");
            1
        }
    }
}

fn interact(events: &Events, state: &mut State, stdout: &mut dyn Write) -> Result<(), String> {
    let mut next_refresh = Instant::now() + REFRESH_INTERVAL;
    let mut input = [0u8; 64];
    loop {
        render(stdout, state).map_err(|error| error.to_string())?;
        let timeout = next_refresh.saturating_duration_since(Instant::now());
        let [input_ready, _] = events.wait_with(&[libc::STDIN_FILENO], Some(timeout));
        if events.take_signal().is_some() {
            return Ok(());
        }
        if input_ready {
            let read =
                unsafe { libc::read(libc::STDIN_FILENO, input.as_mut_ptr().cast(), input.len()) };
            let Ok(read) = usize::try_from(read) else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("read terminal: {error}"));
            };
            if read == 0 {
                return Ok(());
            }
            for key in keys(&input[..read]) {
                match state.handle(key) {
                    Action::Quit => return Ok(()),
                    Action::Refresh => {
                        state.force_scan = true;
                        next_refresh = Instant::now();
                    }
                    Action::RefreshSoon => {
                        state.force_scan = true;
                        next_refresh = Instant::now() + AFTER_STOP_REFRESH;
                    }
                    Action::None => {}
                }
            }
        }
        if Instant::now() >= next_refresh {
            state.refresh();
            next_refresh = Instant::now() + REFRESH_INTERVAL;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Home,
    End,
    Stop,
    Refresh,
    Yes,
    Quit,
    Escape,
    Other,
}

fn keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let rest = &bytes[index..];
        let (key, used) = match rest {
            [0x1b, b'[' | b'O', b'A', ..] => (Key::Up, 3),
            [0x1b, b'[' | b'O', b'B', ..] => (Key::Down, 3),
            [0x1b, b'[' | b'O', b'H', ..] => (Key::Home, 3),
            [0x1b, b'[' | b'O', b'F', ..] => (Key::End, 3),
            [0x1b, b'[', b'3', b'~', ..] => (Key::Stop, 4),
            [0x1b, b'[', ..] => (Key::Other, csi_length(rest)),
            [0x1b, ..] => (Key::Escape, 1),
            [b'k', ..] => (Key::Up, 1),
            [b'j', ..] => (Key::Down, 1),
            [b'g', ..] => (Key::Home, 1),
            [b'G', ..] => (Key::End, 1),
            [b'x' | b'd', ..] => (Key::Stop, 1),
            [b'r', ..] => (Key::Refresh, 1),
            [b'y' | b'Y', ..] => (Key::Yes, 1),
            [b'q' | 0x03 | 0x04, ..] => (Key::Quit, 1),
            _ => (Key::Other, 1),
        };
        keys.push(key);
        index += used;
    }
    keys
}

/// Length of an unrecognized CSI sequence: ESC [ params final-byte.
fn csi_length(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .skip(2)
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|position| position + 3)
        .unwrap_or(bytes.len())
}

enum Action {
    None,
    Refresh,
    RefreshSoon,
    Quit,
}

impl State {
    /// Rescans processes only when the kernel's listener list changed, a
    /// rescan was requested, or the last one is older than FULL_SCAN_INTERVAL.
    fn refresh(&mut self) {
        // A failed scan keeps the last rows on screen and says why.
        let signature = match discovery::listen_signature() {
            Ok(signature) => signature,
            Err(error) => {
                self.message = error;
                return;
            }
        };
        let fresh = self
            .scanned_at
            .is_some_and(|at| at.elapsed() < FULL_SCAN_INTERVAL);
        if !self.force_scan && fresh && signature == self.signature {
            return;
        }
        let listeners = match discovery::all_listeners() {
            Ok(listeners) => listeners,
            Err(error) => {
                self.message = error;
                return;
            }
        };
        self.signature = signature;
        self.scanned_at = Some(Instant::now());
        self.force_scan = false;
        let processes = discovery::process_table();
        // Serve status only changes when listeners do; skip the CLI otherwise.
        let key: BTreeSet<(u32, u16)> = listeners.iter().map(|l| (l.pid, l.port)).collect();
        if key != self.routes_key || self.tailscale_note.is_some() {
            match load_routes() {
                Ok(routes) => {
                    self.routes = routes;
                    self.tailscale_note = None;
                }
                Err(error) => {
                    self.routes.clear();
                    self.tailscale_note = Some(error);
                }
            }
            self.routes_key = key;
        }
        let selected_pid = self.rows.get(self.selected).map(|row| row.pid);
        self.rows = build_rows(&processes, &listeners, &self.routes, self.own_pid);
        let pids: Vec<u32> = self.rows.iter().map(|row| row.pid).collect();
        let directories = discovery::working_directories(&pids);
        let home = std::env::var("HOME").unwrap_or_default();
        for row in &mut self.rows {
            if let Some(dir) = directories.get(&row.pid) {
                row.dir = home_relative(dir, &home);
            }
        }
        let live: BTreeSet<u32> = processes.iter().map(|process| process.pid).collect();
        self.stopped.retain(|pid| live.contains(pid));
        if let Some((pid, _)) = self.pending {
            if !self.rows.iter().any(|row| row.pid == pid) {
                self.pending = None;
            }
        }
        self.selected = selected_pid
            .and_then(|pid| self.rows.iter().position(|row| row.pid == pid))
            .unwrap_or(self.selected)
            .min(self.rows.len().saturating_sub(1));
    }

    fn handle(&mut self, key: Key) -> Action {
        if let Some((pid, pending)) = self.pending.take() {
            if key != Key::Yes {
                self.message = "Cancelled.".to_string();
                return Action::None;
            }
            let Some(row) = self.rows.iter().find(|row| row.pid == pid).cloned() else {
                self.message = format!("{pid} already exited.");
                return Action::Refresh;
            };
            self.message = stop(&row, pending, self.own_pid);
            self.stopped.insert(pid);
            return Action::RefreshSoon;
        }
        let last = self.rows.len().saturating_sub(1);
        match key {
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = (self.selected + 1).min(last),
            Key::Home => self.selected = 0,
            Key::End => self.selected = last,
            Key::Refresh => {
                self.message.clear();
                return Action::Refresh;
            }
            Key::Quit | Key::Escape => return Action::Quit,
            Key::Stop => {
                let Some(row) = self.rows.get(self.selected) else {
                    return Action::None;
                };
                // A Poros session escalates to SIGKILL for its own children
                // and removes its Serve route; killing it outright would
                // orphan both.
                let kind = if self.stopped.contains(&row.pid) && !row.session {
                    Pending::Kill
                } else {
                    Pending::Stop
                };
                let verb = match kind {
                    Pending::Stop => "Stop",
                    Pending::Kill => "Force-kill",
                };
                self.message = format!("{verb} {} ({})? y/N", row.pid, first_word(&row.command));
                self.pending = Some((row.pid, kind));
            }
            Key::Yes | Key::Other => {}
        }
        Action::None
    }
}

fn load_routes() -> Result<Vec<(u16, String)>, String> {
    let tailscale = tailscale::Tailscale::connect(None)?;
    Ok(tailscale.serve_config()?.loopback_routes())
}

fn stop(row: &Row, pending: Pending, own_pid: u32) -> String {
    if row.pid == own_pid || row.pid <= 1 {
        return format!("Refusing to signal {}.", row.pid);
    }
    let (signal, name) = match pending {
        Pending::Stop => (libc::SIGTERM, "SIGTERM"),
        Pending::Kill => (libc::SIGKILL, "SIGKILL"),
    };
    let own_group = u32::try_from(unsafe { libc::getpgrp() }).ok();
    let group = row.group && Some(row.pid) != own_group;
    let sent = if group {
        crate::signal::signal_process_group(row.pid, signal)
    } else {
        crate::signal::signal_pid(row.pid, signal)
    };
    if !sent {
        return format!(
            "Could not signal {}: {}",
            row.pid,
            std::io::Error::last_os_error()
        );
    }
    let target = if group { "group" } else { "process" };
    match pending {
        Pending::Stop if !row.session => format!(
            "Sent {name} to {target} {}. Press x again to force-kill.",
            row.pid
        ),
        _ => format!("Sent {name} to {target} {}.", row.pid),
    }
}

fn build_rows(
    processes: &[ProcessInfo],
    listeners: &[Listener],
    routes: &[(u16, String)],
    own_pid: u32,
) -> Vec<Row> {
    let by_pid: HashMap<u32, &ProcessInfo> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for process in processes {
        if process.ppid != process.pid {
            children.entry(process.ppid).or_default().push(process.pid);
        }
    }
    let mut ports_by_pid: HashMap<u32, Vec<&Listener>> = HashMap::new();
    for listener in listeners {
        if listener.pid != own_pid {
            ports_by_pid.entry(listener.pid).or_default().push(listener);
        }
    }
    let urls_for = |pid: u32| -> Vec<String> {
        let mut urls: Vec<String> = ports_by_pid
            .get(&pid)
            .into_iter()
            .flatten()
            .filter(|listener| listener.ip.is_loopback() || listener.ip.is_unspecified())
            .flat_map(|listener| {
                routes
                    .iter()
                    .filter(move |(port, _)| *port == listener.port)
                    .map(|(_, url)| url.clone())
            })
            .collect();
        urls.sort();
        urls.dedup();
        urls
    };

    let mut rows = Vec::new();
    let mut consumed: BTreeSet<u32> = BTreeSet::new();
    // Sessions first: a served listener whose descendants also listen is a
    // proxy in front of them (Poros). Show one row for the whole tree.
    let mut served: Vec<u32> = ports_by_pid
        .keys()
        .copied()
        .filter(|pid| !urls_for(*pid).is_empty())
        .collect();
    served.sort_unstable();
    for pid in served {
        let descendants: Vec<u32> = descendants(pid, &children)
            .into_iter()
            .filter(|child| ports_by_pid.contains_key(child) && !consumed.contains(child))
            .collect();
        if descendants.is_empty() {
            continue;
        }
        let mut endpoint_listeners: Vec<&Listener> = descendants
            .iter()
            .flat_map(|child| ports_by_pid.get(child).into_iter().flatten().copied())
            .collect();
        endpoint_listeners.sort_by_key(|listener| listener.port);
        consumed.insert(pid);
        consumed.extend(descendants);
        rows.push(Row {
            pid,
            group: false,
            session: true,
            command: command_of(by_pid.get(&pid).copied()),
            dir: String::new(),
            first_port: endpoint_listeners
                .first()
                .map_or(0, |listener| listener.port),
            endpoints: endpoints(&endpoint_listeners),
            urls: urls_for(pid),
        });
    }
    for (pid, listeners) in &ports_by_pid {
        if consumed.contains(pid) {
            continue;
        }
        let mut sorted = listeners.clone();
        sorted.sort_by_key(|listener| listener.port);
        let process = by_pid.get(pid).copied();
        rows.push(Row {
            pid: *pid,
            group: process.is_some_and(|process| process.pgid == *pid),
            session: false,
            command: command_of(process),
            dir: String::new(),
            first_port: sorted.first().map_or(0, |listener| listener.port),
            endpoints: endpoints(&sorted),
            urls: urls_for(*pid),
        });
    }
    rows.sort_by_key(|row| (row.first_port, row.pid));
    rows
}

fn descendants(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut found = Vec::new();
    let mut seen: BTreeSet<u32> = BTreeSet::from([root]);
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if seen.insert(*child) {
                found.push(*child);
                frontier.push(*child);
            }
        }
    }
    found
}

/// The command line with the program reduced to its file name; store and
/// install paths carry no signal in a narrow terminal.
fn command_of(process: Option<&ProcessInfo>) -> String {
    match process {
        Some(process) if !process.command.is_empty() => {
            let (program, arguments) = process
                .command
                .split_once(' ')
                .unwrap_or((process.command.as_str(), ""));
            let program = program.rsplit('/').next().unwrap_or(program);
            if arguments.is_empty() {
                program.to_string()
            } else {
                format!("{program} {arguments}")
            }
        }
        Some(process) => format!("[{}]", process.name),
        None => "?".to_string(),
    }
}

fn home_relative(path: &str, home: &str) -> String {
    if home.len() > 1 {
        if path == home {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(home).filter(|rest| rest.starts_with('/')) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

fn endpoints(listeners: &[&Listener]) -> Vec<String> {
    let mut seen = Vec::new();
    for listener in listeners {
        let text = endpoint(listener.ip, listener.port);
        if !seen.contains(&text) {
            seen.push(text);
        }
    }
    seen
}

fn endpoint(ip: IpAddr, port: u16) -> String {
    if ip.is_unspecified() {
        return format!("*:{port}");
    }
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => format!("{v4}:{port}"),
            None => format!("[{v6}]:{port}"),
        },
    }
}

fn first_word(command: &str) -> &str {
    let program = command.split_whitespace().next().unwrap_or(command);
    program.rsplit('/').next().unwrap_or(program)
}

struct Columns {
    pid: usize,
    listen: usize,
    url: usize,
    dir: usize,
}

fn columns(rows: &[Row]) -> Columns {
    let widest = |values: &mut dyn Iterator<Item = usize>, header: usize, cap: usize| {
        values.max().unwrap_or(0).max(header).min(cap)
    };
    Columns {
        pid: widest(&mut rows.iter().map(|row| row.pid.to_string().len()), 3, 10),
        listen: widest(
            &mut rows
                .iter()
                .map(|row| row.endpoints.join(" ").chars().count()),
            6,
            32,
        ),
        url: widest(
            &mut rows.iter().map(|row| row.urls.join(" ").chars().count()),
            3,
            56,
        ),
        dir: widest(&mut rows.iter().map(|row| row.dir.chars().count()), 3, 32),
    }
}

fn row_line(row: &Row, columns: &Columns) -> String {
    let url = if row.urls.is_empty() {
        "-".to_string()
    } else {
        row.urls.join(" ")
    };
    format!(
        "{:>pid$}  {:<listen$}  {:<url$}  {:<dir$}  {}",
        row.pid,
        fit(&row.endpoints.join(" "), columns.listen),
        fit(&url, columns.url),
        fit_tail(&row.dir, columns.dir),
        row.command,
        pid = columns.pid,
        listen = columns.listen,
        url = columns.url,
        dir = columns.dir,
    )
}

fn header_line(columns: &Columns) -> String {
    format!(
        "{:>pid$}  {:<listen$}  {:<url$}  {:<dir$}  COMMAND",
        "PID",
        "LISTEN",
        "URL",
        "DIR",
        pid = columns.pid,
        listen = columns.listen,
        url = columns.url,
        dir = columns.dir,
    )
}

/// Truncates to `width` characters, marking the cut with an ellipsis.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Like `fit`, but keeps the end: a directory's last components name it.
fn fit_tail(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    let mut cut = String::from("…");
    cut.extend(text.chars().skip(count + 1 - width.max(1)));
    cut
}

fn print_table(stdout: &mut dyn Write, state: &State) -> Result<(), String> {
    let columns = columns(&state.rows);
    let mut text = header_line(&columns);
    text.push('\n');
    for row in &state.rows {
        text.push_str(row_line(row, &columns).trim_end());
        text.push('\n');
    }
    if let Some(note) = &state.tailscale_note {
        text.push_str(&format!("Tailscale URLs unavailable: {note}\n"));
    }
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())
}

fn render(stdout: &mut dyn Write, state: &mut State) -> std::io::Result<()> {
    let (width, height) = terminal::size();
    // Title, blank, header, rows..., blank, message, keys.
    let visible = height.saturating_sub(6).max(1);
    if state.selected < state.offset {
        state.offset = state.selected;
    } else if state.selected >= state.offset + visible {
        state.offset = state.selected + 1 - visible;
    }
    state.offset = state.offset.min(state.rows.len().saturating_sub(visible));
    let columns = columns(&state.rows);
    let count = state.rows.len();
    let title = format!(
        "poros dashboard: {count} listening server{}",
        if count == 1 { "" } else { "s" }
    );
    let mut frame = String::from("\x1b[H");
    let mut line = |text: &str, style: &str| {
        frame.push_str("\x1b[2K");
        frame.push_str(style);
        frame.push_str(&fit(text, width));
        if !style.is_empty() {
            frame.push_str("\x1b[0m");
        }
        frame.push_str("\r\n");
    };
    line(&title, "\x1b[1m");
    line("", "");
    line(&header_line(&columns), "\x1b[2m");
    if state.rows.is_empty() {
        line("No listening TCP servers owned by this user.", "");
    }
    for (index, row) in state
        .rows
        .iter()
        .enumerate()
        .skip(state.offset)
        .take(visible)
    {
        let style = if index == state.selected {
            "\x1b[7m"
        } else {
            ""
        };
        let text = row_line(row, &columns);
        line(&format!("{text:<width$}"), style);
    }
    line("", "");
    let message = match (&state.tailscale_note, state.message.is_empty()) {
        (_, false) => state.message.clone(),
        (Some(note), true) => format!("Tailscale URLs unavailable: {note}"),
        (None, true) => String::new(),
    };
    line(&message, "");
    frame.push_str("\x1b[2K\x1b[2m");
    frame.push_str(&fit(
        "↑/↓ or j/k select   x stop   r refresh   q quit",
        width,
    ));
    frame.push_str("\x1b[0m\x1b[J");
    stdout.write_all(frame.as_bytes())?;
    stdout.flush()
}

mod terminal {
    /// Raw-mode alternate screen; restored on drop, including during panics.
    pub struct Raw {
        original: libc::termios,
    }

    pub fn is_tty() -> bool {
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
    }

    impl Raw {
        pub fn enter() -> Result<Self, String> {
            let mut original: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
                return Err(format!(
                    "read terminal mode: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut raw = original;
            // Keep ISIG: Ctrl-C arrives as SIGINT and ends the loop cleanly.
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN);
            raw.c_iflag &= !(libc::IXON | libc::ICRNL);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
                return Err(format!(
                    "set terminal mode: {}",
                    std::io::Error::last_os_error()
                ));
            }
            write_str("\x1b[?1049h\x1b[?25l");
            Ok(Self { original })
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            write_str("\x1b[?25h\x1b[?1049l");
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
            }
        }
    }

    fn write_str(text: &str) {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }

    /// Columns and rows; 80x24 when the size cannot be read.
    pub fn size() -> (usize, usize) {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
        if ok && size.ws_col > 0 && size.ws_row > 0 {
            (usize::from(size.ws_col), usize::from(size.ws_row))
        } else {
            (80, 24)
        }
    }
}
