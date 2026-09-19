use crate::cli::{parse_config, Config, ParseOutcome};
use crate::discovery;
use crate::proxy::Bridge;
use crate::serve::{serve_command, RESERVED_FUNNEL_PORTS};
use crate::tailscale;
use std::io::Write;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const TERMINATE_GRACE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(150);
const MAX_SERVE_ATTEMPTS: usize = 4;

/// Spawned dev command in its own process group.
struct ChildProcess {
    process: Child,
    pid: u32,
}

/// Foreground `tailscale serve` process plus its captured output.
struct ServeProcess {
    process: Child,
    output: Option<std::thread::JoinHandle<String>>,
}

impl ServeProcess {
    /// None while running; Some(output) once the CLI exited.
    fn poll_exit(&mut self) -> Option<String> {
        match self.process.try_wait() {
            Ok(Some(_)) => {
                let _ = self.process.wait();
                let output = self
                    .output
                    .take()
                    .and_then(|handle| handle.join().ok())
                    .unwrap_or_default();
                Some(output)
            }
            Ok(None) => None,
            Err(_) => Some(String::new()),
        }
    }

    /// SIGINT lets tailscale remove its own foreground session; the kill is
    /// only the fallback for a wedged CLI.
    fn stop(&mut self) -> Option<String> {
        unsafe {
            let _ = libc::kill(self.process.id() as i32, libc::SIGINT);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match self.process.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        if self.process.try_wait().ok().flatten().is_none() {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
        self.output.take().and_then(|handle| handle.join().ok())
    }
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
    let signals = match crate::signal::install() {
        Ok(receiver) => receiver,
        Err(error) => {
            let _ = writeln!(stderr, "poros: {error}");
            return 1;
        }
    };

    let outcome = supervise(&c, &signals, &mut stdout);
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

fn supervise(
    c: &Config,
    signals: &mpsc::Receiver<i32>,
    stdout: &mut dyn Write,
) -> Result<(), Failure> {
    let _stderr = std::io::stderr();
    let cli = tailscale::find_cli(c.tailscale_cli.as_deref()).map_err(Failure::error)?;
    let node = tailscale::load_node(&cli).map_err(Failure::error)?;
    let initial = tailscale::read_serve_config(&cli).map_err(Failure::error)?;
    let bridge_port = reserve_bridge_port().map_err(Failure::error)?;
    let https_port = if c.port == 0 { bridge_port } else { c.port };
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
    let deadline = Instant::now() + c.timeout;

    // Resolve the loopback target before the bridge starts.
    let target = match &c.target {
        Some(target) => target.clone(),
        None => {
            let Some(root_pid) = child_state.as_ref().map(|child| child.pid) else {
                return Err(Failure::error("provide a command or --target"));
            };
            let mut resolved: Option<crate::cli::TargetUrl> = None;
            while resolved.is_none() {
                if let Some(signal_number) = take_signal(signals) {
                    terminate_child(&mut child_state, signal_number);
                    return Err(Failure::plain(128 + signal_number));
                }
                if let Some(code) = child_exit_code(&mut child_state) {
                    if let Some(pid) = child_state.as_ref().map(|child| child.pid) {
                        stop_remaining_group(pid);
                    }
                    return Err(Failure::plain(code));
                }
                match discovery::discover_target(root_pid) {
                    Ok(Some(target)) => resolved = Some(target),
                    Ok(None) => {}
                    Err(error) => {
                        terminate_child(&mut child_state, libc::SIGTERM);
                        return Err(Failure::error(error));
                    }
                }
                if resolved.is_none() {
                    if Instant::now() >= deadline {
                        terminate_child(&mut child_state, libc::SIGTERM);
                        return Err(Failure::error(
							"no unique loopback HTTP listener found before timeout; bind to localhost or select --target",
						));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
            resolved.expect("target resolved")
        }
    };

    let authority = format!("{}:{}", node.dns_name, https_port);
    let (bridge, bridge_actual) =
        Bridge::bind(target.clone(), authority.clone()).map_err(Failure::error)?;
    let shutdown_flag = bridge.shutdown_handle();
    let bridge_thread = std::thread::Builder::new()
        .name("poros-bridge".to_string())
        .spawn(move || bridge.serve())
        .expect("bridge thread spawns");
    let bridge_url = format!("http://127.0.0.1:{bridge_actual}");

    let run_result = supervise_loop(
        c,
        signals,
        stdout,
        &cli,
        https_port,
        &authority,
        &bridge_url,
        &target,
        deadline,
        &mut child_state,
        &shutdown_flag,
        &bridge_thread,
    );

    shutdown_flag.store(true, Ordering::SeqCst);
    let _ = bridge_thread.join();
    run_result
}

#[allow(clippy::too_many_arguments)]
fn supervise_loop(
    _c: &Config,
    signals: &mpsc::Receiver<i32>,
    stdout: &mut dyn Write,
    cli: &std::path::Path,
    https_port: u16,
    authority: &str,
    bridge_url: &str,
    target: &crate::cli::TargetUrl,
    deadline: Instant,
    child_state: &mut Option<ChildProcess>,
    _shutdown_flag: &Arc<std::sync::atomic::AtomicBool>,
    _bridge_thread: &std::thread::JoinHandle<()>,
) -> Result<(), Failure> {
    let mut stderr = std::io::stderr();
    let mut serve_state: Option<ServeProcess> = None;
    let mut serve_attempt: usize = 0;
    let mut serve_ready = false;
    let _serve_output: Option<String> = None;
    let mut retry_at: Option<Instant> = None;

    start_serve(cli, https_port, bridge_url, &mut serve_state).map_err(Failure::error)?;
    serve_attempt += 1;

    loop {
        if let Some(signal_number) = take_signal(signals) {
            terminate_child(child_state, signal_number);
            let output = stop_serve(&mut serve_state);
            print_serve_output(&mut stderr, output, serve_ready);
            return Err(Failure::plain(128 + signal_number));
        }
        if let Some(code) = child_exit_code(child_state) {
            if let Some(pid) = child_state.as_ref().map(|child| child.pid) {
                stop_remaining_group(pid);
            }
            let output = stop_serve(&mut serve_state);
            print_serve_output(&mut stderr, output, serve_ready);
            return Err(Failure::plain(code));
        }
        if let Some(when) = retry_at {
            if Instant::now() >= when {
                retry_at = None;
                start_serve(cli, https_port, bridge_url, &mut serve_state)
                    .map_err(Failure::error)?;
                serve_attempt += 1;
            }
        }
        if let Some(serve) = serve_state.as_mut() {
            if let Some(output) = serve.poll_exit() {
                serve_state = None;
                let recoverable = !serve_ready
                    && serve_attempt < MAX_SERVE_ATTEMPTS
                    && output.contains("etag mismatch");
                if recoverable {
                    let delay = 100u64 * serve_attempt as u64;
                    retry_at = Some(Instant::now() + Duration::from_millis(delay));
                    continue;
                }
                terminate_child(child_state, libc::SIGTERM);
                if serve_ready {
                    return Err(Failure::plain(1));
                }
                let _ = write!(stderr, "{output}");
                return Err(Failure::error(
                    "Tailscale Serve stopped; check HTTPS setup, permissions, and port conflicts",
                ));
            }
        }
        if !serve_ready {
            if Instant::now() >= deadline {
                terminate_child(child_state, libc::SIGTERM);
                let output = stop_serve(&mut serve_state);
                print_serve_output(&mut stderr, output, serve_ready);
                return Err(Failure::error(
                    "Tailscale Serve startup timed out; check HTTPS setup and permissions",
                ));
            }
            if serve_state.is_some() {
                let status = tailscale::read_serve_config(cli).map_err(Failure::error)?;
                if status.owns_route(authority, bridge_url) {
                    serve_ready = true;
                    writeln!(
                        stdout,
                        "Poros URL: https://{authority}/\nLocal target: {}",
                        target.url()
                    )
                    .map_err(|error| Failure::error(error.to_string()))?;
                    stdout
                        .flush()
                        .map_err(|error| Failure::error(error.to_string()))?;
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn print_serve_output(stderr: &mut dyn Write, output: Option<String>, serve_ready: bool) {
    // Normal CLI route announcements and recoverable conflicts stay hidden;
    // only unexplained exits before readiness are surfaced.
    if !serve_ready {
        if let Some(output) = output {
            let _ = write!(stderr, "{output}");
        }
    }
}

fn reserve_bridge_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    drop(listener);
    Ok(port)
}

fn take_signal(receiver: &mpsc::Receiver<i32>) -> Option<i32> {
    receiver.try_recv().ok().filter(|signal| *signal != 0)
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
    builder.stdin(std::process::Stdio::inherit());
    builder.stdout(std::process::Stdio::inherit());
    builder.stderr(std::process::Stdio::inherit());
    unsafe {
        builder.pre_exec(|| {
            // The child becomes its own process group leader so Poros can
            // signal the whole tree without touching unrelated processes.
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
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
    Ok(ChildProcess { process, pid })
}

fn start_serve(
    cli: &std::path::Path,
    https_port: u16,
    bridge: &str,
    serve_state: &mut Option<ServeProcess>,
) -> Result<(), String> {
    let mut command = serve_command(cli, https_port, bridge);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    serve_state.replace(start_serve_process(command)?);
    Ok(())
}

fn start_serve_process(mut command: Command) -> Result<ServeProcess, String> {
    let mut process = command
        .spawn()
        .map_err(|error| format!("start Tailscale Serve: {error}"))?;
    let (sender, receiver) = mpsc::channel::<Vec<u8>>();
    let stdout = process.stdout.take();
    let stderr = process.stderr.take();
    std::thread::spawn(move || {
        if let Some(mut stream) = stdout {
            capture_into(&mut stream, &sender);
        }
        if let Some(mut stream) = stderr {
            capture_into(&mut stream, &sender);
        }
        drop(sender);
    });
    let collector = std::thread::spawn(move || {
        let mut output = String::new();
        for chunk in receiver {
            output.push_str(&String::from_utf8_lossy(&chunk));
        }
        output
    });
    Ok(ServeProcess {
        process,
        output: Some(collector),
    })
}

fn capture_into(stream: &mut dyn std::io::Read, sender: &mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if sender.send(buffer[..read].to_vec()).is_err() {
                    return;
                }
            }
        }
    }
}

fn stop_serve(serve_state: &mut Option<ServeProcess>) -> Option<String> {
    serve_state.as_mut().and_then(ServeProcess::stop)
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
