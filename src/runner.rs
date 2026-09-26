use crate::cli::{parse_config, Config, ParseOutcome, TargetUrl};
use crate::discovery;
use crate::localapi::Session;
use crate::proxy::Bridge;
use crate::serve::{serve_command, RESERVED_FUNNEL_PORTS};
use crate::signal::Events;
use crate::tailscale::{self, Tailscale};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::fd::RawFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TERMINATE_GRACE: Duration = Duration::from_secs(5);
/// Startup-only cadence for listener discovery and CLI readiness checks.
/// Once the URL is printed, Poros sleeps until a signal, child exit, or
/// connection arrives.
const POLL_INTERVAL: Duration = Duration::from_millis(150);
const MAX_SERVE_ATTEMPTS: usize = 4;

/// Spawned dev command in its own process group.
struct ChildProcess {
    process: Child,
    pid: u32,
    /// The group was made the terminal's foreground group; Poros takes the
    /// terminal back once the command is gone.
    terminal: bool,
}

/// Foreground `tailscale serve` CLI, used only without a LocalAPI socket.
/// Its output goes to an unlinked temporary file, read only after it exits:
/// no capture threads and no buffered copy while it runs.
struct ServeProcess {
    process: Child,
    output: File,
}

impl ServeProcess {
    fn start(cli: &Path, https_port: u16, bridge: &str) -> Result<Self, String> {
        let output = unlinked_temp_file()?;
        let stdout = output
            .try_clone()
            .map_err(|error| format!("start Tailscale Serve: {error}"))?;
        let stderr = output
            .try_clone()
            .map_err(|error| format!("start Tailscale Serve: {error}"))?;
        let process = serve_command(cli, https_port, bridge)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|error| format!("start Tailscale Serve: {error}"))?;
        Ok(Self { process, output })
    }

    fn read_output(&mut self) -> String {
        let mut bytes = Vec::new();
        let read = self
            .output
            .rewind()
            .and_then(|()| self.output.read_to_end(&mut bytes));
        match read {
            Ok(_) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => format!("(could not read Tailscale Serve output: {error})\n"),
        }
    }

    /// None while running; Some(output) once the CLI exited.
    fn poll_exit(&mut self) -> Option<String> {
        match self.process.try_wait() {
            Ok(Some(_)) | Err(_) => Some(self.read_output()),
            Ok(None) => None,
        }
    }

    /// SIGINT lets tailscale remove its own foreground session; the kill is
    /// only the fallback for a wedged CLI.
    fn stop(&mut self) -> String {
        if let Ok(pid) = i32::try_from(self.process.id()) {
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(self.process.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if matches!(self.process.try_wait(), Ok(None)) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
        self.read_output()
    }
}

/// A private file with no name: created exclusively (0600), then unlinked.
fn unlinked_temp_file() -> Result<File, String> {
    let directory = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    for attempt in 0..16u32 {
        let path = directory.join(format!(
            "poros-serve-{}-{stamp}-{attempt}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(&path).map_err(|error| {
                    format!("remove temporary file {}: {error}", path.display())
                })?;
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create temporary file: {error}")),
        }
    }
    Err("create temporary file: no unused name".to_string())
}

/// The CLI fallback's lifecycle: start, retry on config races, poll Serve
/// status until the route appears.
struct CliRoute {
    cli: PathBuf,
    https_port: u16,
    bridge_url: String,
    process: Option<ServeProcess>,
    attempts: usize,
    retry_at: Option<Instant>,
}

/// Who keeps the Serve route alive.
enum Route {
    /// tailscaled holds it for as long as this LocalAPI connection is open.
    Local(Session),
    Cli(CliRoute),
}

impl Route {
    fn fd(&self) -> RawFd {
        match self {
            Self::Local(session) => session.fd(),
            Self::Cli(_) => -1,
        }
    }

    /// Ends the route. The CLI's output is shown only if it never became
    /// ready: normal route announcements stay hidden.
    fn stop(self, stderr: &mut dyn Write, ready: bool) {
        match self {
            // Closing the connection makes tailscaled delete the route.
            Self::Local(session) => drop(session),
            Self::Cli(mut route) => {
                if let Some(mut process) = route.process.take() {
                    let output = process.stop();
                    if !ready {
                        let _ = write!(stderr, "{output}");
                    }
                }
            }
        }
    }
}

/// Why supervision ended; cleanup depends on it.
enum Exit {
    Signal(i32),
    ChildExited(i32),
    Failed(Failure),
}

pub fn run(args: Vec<String>) -> i32 {
    let mut stderr = std::io::stderr();
    match parse_config(&args) {
        Ok(ParseOutcome::Help) => {
            print!("{}", crate::cli::usage_text());
            0
        }
        Ok(ParseOutcome::Version(version)) => {
            println!("{version}");
            0
        }
        Ok(ParseOutcome::Config(config)) => run_config(config),
        Ok(ParseOutcome::Dashboard) => crate::dashboard::run(),
        Err(error) => {
            let _ = writeln!(stderr, "poros: {error}");
            2
        }
    }
}

struct Failure {
    code: i32,
    message: Option<String>,
}

impl Failure {
    fn error(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: Some(message.into()),
        }
    }

    fn plain(code: i32) -> Self {
        Self {
            code,
            message: None,
        }
    }
}

fn run_config(c: Config) -> i32 {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let events = match Events::install(&[]) {
        Ok(events) => events,
        Err(error) => {
            let _ = writeln!(stderr, "poros: {error}");
            return 1;
        }
    };

    let outcome = supervise(&c, &events, &mut stdout);
    match outcome {
        Ok(()) => 0,
        Err(failure) => {
            if let Some(message) = failure.message {
                if crate::signal::last_signal() == 0 {
                    let _ = writeln!(stderr, "poros: {message}");
                }
            }
            if failure.code == 0 {
                1
            } else {
                failure.code
            }
        }
    }
}

fn supervise(c: &Config, events: &Events, stdout: &mut dyn Write) -> Result<(), Failure> {
    let tailscale = Tailscale::connect(c.tailscale_cli.as_deref()).map_err(Failure::error)?;
    let node = tailscale.node().map_err(Failure::error)?;
    let initial = tailscale.serve_config().map_err(Failure::error)?;
    let https_port = if c.port == 0 {
        reserve_port().map_err(Failure::error)?
    } else {
        c.port
    };
    if RESERVED_FUNNEL_PORTS.contains(&https_port) {
        return Err(Failure::error(
            "choose a dedicated HTTPS port, excluding Funnel ports 443, 8443, and 10000",
        ));
    }
    if initial.uses_port(https_port) {
        return Err(Failure::error(format!(
			"HTTPS port {https_port} is already configured in Tailscale Serve; choose another with --https"
		)));
    }
    drop(initial);
    if c.target.is_none() && !c.command.is_empty() {
        if let Some(tool) = discovery::required_tools_missing() {
            return Err(Failure::error(format!(
                "automatic discovery requires {tool}"
            )));
        }
    }

    // Spawn the dev command in its own process group.
    let mut child_state: Option<ChildProcess> = None;
    if !c.command.is_empty() {
        child_state = Some(spawn_child(&c.command).map_err(Failure::error)?);
    }
    let run = Run {
        events,
        tailscale: &tailscale,
        authority: format!("{}:{}", node.dns_name, https_port),
        https_port,
        deadline: Instant::now() + c.timeout,
    };
    let terminal = child_state.as_ref().is_some_and(|child| child.terminal);
    let (exit, route) = run.execute(stdout, c.target.clone(), &mut child_state);
    // Stop the dev command first, then remove its route, as before.
    let failure = match exit {
        Exit::Signal(signal_number) => {
            terminate_child(&mut child_state, signal_number);
            Failure::plain(128 + signal_number)
        }
        Exit::ChildExited(code) => {
            if let Some(pid) = child_state.as_ref().map(|child| child.pid) {
                stop_remaining_group(pid);
            }
            Failure::plain(code)
        }
        Exit::Failed(failure) => {
            terminate_child(&mut child_state, libc::SIGTERM);
            failure
        }
    };
    if terminal {
        hand_terminal(unsafe { libc::getpgrp() });
    }
    if let Some((route, ready)) = route {
        route.stop(&mut std::io::stderr(), ready);
    }
    Err(failure)
}

/// One run's fixed parameters.
struct Run<'a> {
    events: &'a Events,
    tailscale: &'a Tailscale,
    authority: String,
    https_port: u16,
    deadline: Instant,
}

impl Run<'_> {
    /// Resolves the target, publishes the route, and supervises until
    /// something ends the run. Returns the route (and whether it became
    /// ready) so the caller removes it after stopping the dev command.
    fn execute(
        &self,
        stdout: &mut dyn Write,
        target: Option<TargetUrl>,
        child_state: &mut Option<ChildProcess>,
    ) -> (Exit, Option<(Route, bool)>) {
        let target = match target {
            Some(target) => target,
            None => match self.discover(child_state) {
                Ok(target) => target,
                Err(exit) => return (exit, None),
            },
        };
        let (bridge, bridge_port) = match Bridge::bind(&target, self.authority.clone()) {
            Ok(bound) => bound,
            Err(error) => return (failed(error), None),
        };
        if let Err(error) = bridge.start() {
            return (failed(error), None);
        }
        let bridge_url = format!("http://127.0.0.1:{bridge_port}");
        let route = match self.tailscale {
            Tailscale::Local(api) => api
                .open_route(self.https_port, &self.authority, &bridge_url)
                .map(Route::Local),
            Tailscale::Cli(cli) => {
                ServeProcess::start(cli, self.https_port, &bridge_url).map(|process| {
                    Route::Cli(CliRoute {
                        cli: cli.clone(),
                        https_port: self.https_port,
                        bridge_url,
                        process: Some(process),
                        attempts: 1,
                        retry_at: None,
                    })
                })
            }
        };
        let mut route = match route {
            Ok(route) => route,
            Err(error) => return (failed(error), None),
        };
        let mut ready = false;
        let exit = self.supervise(stdout, &mut route, &target, child_state, &mut ready);
        (exit, Some((route, ready)))
    }

    /// Polls the child's processes for its loopback listener.
    fn discover(&self, child_state: &mut Option<ChildProcess>) -> Result<TargetUrl, Exit> {
        let Some(root_pid) = child_state.as_ref().map(|child| child.pid) else {
            return Err(failed("provide a command or --target"));
        };
        loop {
            if let Some(signal_number) = self.events.take_signal() {
                return Err(Exit::Signal(signal_number));
            }
            if let Some(code) = child_exit_code(child_state) {
                return Err(Exit::ChildExited(code));
            }
            match discovery::discover_target(root_pid) {
                Ok(Some(target)) => return Ok(target),
                Ok(None) => {}
                Err(error) => return Err(failed(error)),
            }
            if Instant::now() >= self.deadline {
                return Err(failed(
                    "no unique loopback HTTP listener found before timeout; bind to localhost or select --target",
                ));
            }
            self.events.wait(Some(POLL_INTERVAL));
        }
    }

    /// Sleeps in poll(2) on the signal pipe and the LocalAPI session: every
    /// wakeup is real work. Bridge workers accept connections on their own.
    fn supervise(
        &self,
        stdout: &mut dyn Write,
        route: &mut Route,
        target: &TargetUrl,
        child_state: &mut Option<ChildProcess>,
        ready: &mut bool,
    ) -> Exit {
        if matches!(route, Route::Local(_)) {
            if let Err(failure) = announce(stdout, &self.authority, target) {
                return Exit::Failed(failure);
            }
            *ready = true;
        }
        let mut route_readable = false;
        loop {
            if let Some(signal_number) = self.events.take_signal() {
                return Exit::Signal(signal_number);
            }
            if let Some(code) = child_exit_code(child_state) {
                return Exit::ChildExited(code);
            }
            let timeout = match route {
                Route::Local(session) => {
                    if route_readable && !session.drain() {
                        return failed(
                            "Tailscale ended the Serve session: tailscaled restarted or its Serve config was replaced",
                        );
                    }
                    None
                }
                Route::Cli(cli) => match cli.step(stdout, self, target, ready) {
                    Ok(timeout) => timeout,
                    Err(exit) => return exit,
                },
            };
            let [route_ready, _] = self.events.wait_with(&[route.fd()], timeout);
            route_readable = route_ready;
        }
    }
}

impl CliRoute {
    /// Advances the CLI's lifecycle; returns how long to sleep at most.
    fn step(
        &mut self,
        stdout: &mut dyn Write,
        run: &Run<'_>,
        target: &TargetUrl,
        ready: &mut bool,
    ) -> Result<Option<Duration>, Exit> {
        let now = Instant::now();
        if self.retry_at.is_some_and(|when| now >= when) {
            self.retry_at = None;
            self.process = Some(
                ServeProcess::start(&self.cli, self.https_port, &self.bridge_url)
                    .map_err(failed)?,
            );
            self.attempts += 1;
        }
        if let Some(output) = self.process.as_mut().and_then(ServeProcess::poll_exit) {
            self.process = None;
            let recoverable =
                !*ready && self.attempts < MAX_SERVE_ATTEMPTS && output.contains("etag mismatch");
            if recoverable {
                let delay = Duration::from_millis(100 * self.attempts as u64);
                self.retry_at = Some(now + delay);
                return Ok(Some(delay));
            }
            if *ready {
                return Err(Exit::Failed(Failure::plain(1)));
            }
            eprint!("{output}");
            return Err(failed(
                "Tailscale Serve stopped; check HTTPS setup, permissions, and port conflicts",
            ));
        }
        if let Some(when) = self.retry_at {
            return Ok(Some(when.saturating_duration_since(now)));
        }
        if *ready {
            return Ok(None);
        }
        if now >= run.deadline {
            return Err(failed(
                "Tailscale Serve startup timed out; check HTTPS setup and permissions",
            ));
        }
        let status = tailscale::read_serve_config(&self.cli).map_err(failed)?;
        if status.owns_route(&run.authority, &self.bridge_url) {
            announce(stdout, &run.authority, target).map_err(Exit::Failed)?;
            *ready = true;
            return Ok(None);
        }
        Ok(Some(POLL_INTERVAL))
    }
}

fn failed(message: impl Into<String>) -> Exit {
    Exit::Failed(Failure::error(message))
}

fn announce(stdout: &mut dyn Write, authority: &str, target: &TargetUrl) -> Result<(), Failure> {
    writeln!(
        stdout,
        "Poros URL: https://{authority}/\nLocal target: {}",
        target.url()
    )
    .and_then(|()| stdout.flush())
    .map_err(|error| Failure::error(error.to_string()))
}

fn reserve_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    drop(listener);
    Ok(port)
}

fn child_exit_code(child_state: &mut Option<ChildProcess>) -> Option<i32> {
    let child = child_state.as_mut()?;
    match child.process.try_wait() {
        Ok(Some(status)) => Some(status.code().unwrap_or(1)),
        Ok(None) => None,
        Err(_) => Some(1),
    }
}

fn spawn_child(command: &[String]) -> Result<ChildProcess, String> {
    let Some(program) = command.first() else {
        return Err("empty command".to_string());
    };
    let mut builder = Command::new(program);
    builder.args(&command[1..]);
    builder.stdin(Stdio::inherit());
    builder.stdout(Stdio::inherit());
    builder.stderr(Stdio::inherit());
    // A dev server that reads its terminal (Vite's shortcuts) must own it: a
    // background group that touches the terminal is stopped by SIGTTIN.
    let terminal = unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1
            && libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp()
    };
    unsafe {
        builder.pre_exec(move || {
            // The child becomes its own process group leader so Poros can
            // signal the whole tree without touching unrelated processes.
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if terminal {
                hand_terminal(libc::getpgrp());
            }
            Ok(())
        });
    }
    let mut process = builder.spawn().map_err(|error| error.to_string())?;
    let pid = process.id();
    // The parent half of setpgid guarantees the group exists before any signal.
    let Ok(pid_i32) = i32::try_from(pid) else {
        let _ = process.kill();
        let _ = process.wait();
        return Err("child pid out of range".to_string());
    };
    unsafe {
        let _ = libc::setpgid(pid_i32, pid_i32);
    }
    // Both halves hand over the terminal, so neither order of scheduling
    // lets the child read it from the background.
    if terminal {
        hand_terminal(pid_i32);
    }
    Ok(ChildProcess {
        process,
        pid,
        terminal,
    })
}

/// Makes `group` the terminal's foreground process group. SIGTTOU is blocked
/// meanwhile, since a background caller would otherwise be stopped by it.
/// Async-signal-safe: the child runs it between fork and exec.
fn hand_terminal(group: libc::pid_t) {
    unsafe {
        let mut block: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut block);
        libc::sigaddset(&mut block, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut previous);
        let _ = libc::tcsetpgrp(libc::STDIN_FILENO, group);
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
    }
}

fn terminate_child(child_state: &mut Option<ChildProcess>, signal_number: i32) {
    if let Some(child) = child_state.take() {
        crate::signal::signal_process_group(child.pid, signal_number);
        wait_group_exit(child.pid, TERMINATE_GRACE);
        if crate::signal::process_group_alive(child.pid) {
            crate::signal::signal_process_group(child.pid, libc::SIGKILL);
            wait_group_exit(child.pid, Duration::from_secs(1));
        }
        let mut process = child.process;
        let _ = process.wait();
    }
}

fn stop_remaining_group(process_group_id: u32) {
    if process_group_id == 0 || !crate::signal::process_group_alive(process_group_id) {
        return;
    }
    crate::signal::signal_process_group(process_group_id, libc::SIGTERM);
    wait_group_exit(process_group_id, TERMINATE_GRACE);
    if crate::signal::process_group_alive(process_group_id) {
        crate::signal::signal_process_group(process_group_id, libc::SIGKILL);
        wait_group_exit(process_group_id, Duration::from_secs(1));
    }
}

fn wait_group_exit(process_group_id: u32, deadline: Duration) {
    let end = Instant::now() + deadline;
    while crate::signal::process_group_alive(process_group_id) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
}
