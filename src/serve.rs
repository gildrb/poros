/// Builds the exact `tailscale serve` invocation: one foreground session on a
/// dedicated HTTPS port proxying the loopback bridge.
pub fn serve_command(
    cli: &std::path::Path,
    https_port: u16,
    bridge: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new(cli);
    command
        .arg("serve")
        .arg("--bg=false")
        .arg(format!("--https={https_port}"))
        .arg(bridge);
    command
}

/// HTTPS ports Tailscale reserves for Funnel; Poros never touches them.
pub const RESERVED_FUNNEL_PORTS: [u16; 3] = [443, 8443, 10000];
