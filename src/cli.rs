use std::time::Duration;

/// Loopback HTTP target for the bridge. The address is pre-validated:
/// http scheme, no credentials, no path/query/fragment, loopback host only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetUrl {
    pub host: String,
    pub port: u16,
}

impl TargetUrl {
    /// Canonical http URL for this target, used for display and proxy Host.
    pub fn url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

#[derive(Debug)]
pub struct Config {
    pub command: Vec<String>,
    pub port: u16,
    pub tailscale_cli: Option<String>,
    pub target: Option<TargetUrl>,
    pub version: bool,
    pub timeout: Duration,
}

#[derive(Debug)]
pub enum ParseOutcome {
    Config(Config),
    Dashboard,
    Version(String),
    Help,
}

enum Step {
    End,
    Terminator,
    Flag(String, String),
}

/// Flags that default to true and may appear without a value.
const BOOLEAN_FLAGS: [&str; 3] = ["version", "help", "h"];

struct FlagParser<'a> {
    args: &'a [String],
    index: usize,
}

impl<'a> FlagParser<'a> {
    fn new(args: &'a [String]) -> Self {
        Self { args, index: 0 }
    }

    /// Returns End at end of input, Terminator for "--", or the next flag.
    fn next(&mut self) -> Result<Step, String> {
        if self.index >= self.args.len() {
            return Ok(Step::End);
        }
        let item = &self.args[self.index];
        if item == "--" {
            self.index += 1;
            return Ok(Step::Terminator);
        }
        if !item.starts_with('-') || item == "-" {
            return Ok(Step::End);
        }
        let name = item.trim_start_matches('-');
        if name.is_empty() {
            return Err(format!("bad flag syntax: {item}"));
        }
        let bare_name = name.to_string();
        if let Some(eq) = name.find('=') {
            let (key, value) = name.split_at(eq);
            if key.is_empty() {
                return Err(format!("bad flag syntax: {item}"));
            }
            self.index += 1;
            return Ok(Step::Flag(key.to_string(), value[1..].to_string()));
        }
        self.index += 1;
        if BOOLEAN_FLAGS.contains(&bare_name.as_str()) {
            // Boolean flags default to true; an explicit =value form is handled above.
            return Ok(Step::Flag(bare_name, "true".to_string()));
        }
        if self.index >= self.args.len() {
            return Err(format!("flag needs an argument: -{name}"));
        }
        let value = self.args[self.index].clone();
        self.index += 1;
        Ok(Step::Flag(bare_name, value))
    }

    fn remaining(&self) -> Vec<String> {
        self.args[self.index..].to_vec()
    }
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    let units: &[(&str, u64)] = &[
        ("ns", 0),
        ("us", 0),
        ("µs", 0),
        ("ms", 0),
        ("s", 1_000_000_000),
        ("m", 60_000_000_000),
        ("h", 3_600_000_000_000),
    ];
    let unit_start = text
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(unit_start);
    if number.is_empty() || unit.is_empty() {
        return Err(format!("invalid duration {text:?}"));
    }
    let value: f64 = number
        .parse()
        .map_err(|_| format!("invalid duration {text:?}"))?;
    let multiplier = units
        .iter()
        .find(|(name, _)| *name == unit)
        .map(|(_, multiplier)| *multiplier)
        .ok_or_else(|| format!("unknown unit {unit:?} in duration {text:?}"))?;
    if multiplier == 0 {
        return Err(format!("sub-second durations are not supported: {text:?}"));
    }
    let nanos = value * (multiplier as f64);
    if !(nanos.is_finite()) || nanos < 0.0 || nanos > (u64::MAX as f64) {
        return Err(format!("duration {text:?} out of range"));
    }
    Ok(Duration::from_nanos(nanos as u64))
}

fn parse_target(value: &str) -> Result<TargetUrl, String> {
    let rejected = || {
        "target must be a loopback HTTP URL without credentials, path, query, or fragment"
            .to_string()
    };
    let rest = value.strip_prefix("http://").ok_or_else(rejected)?;
    let authority = match rest.find(['/', '?', '#']) {
        Some(index) => &rest[..index],
        None => rest,
    };
    let path = &rest[authority.len()..];
    if path.starts_with('/') && path != "/" {
        return Err(rejected());
    }
    if path.starts_with('?') || path.starts_with('#') {
        return Err(rejected());
    }
    if authority.is_empty() || authority.contains('@') {
        return Err(rejected());
    }
    let (host_text, port) = if let Some(inner) = authority.strip_prefix('[') {
        let close = inner
            .find(']')
            .ok_or_else(|| format!("invalid IPv6 address in {value:?}"))?;
        let host = &inner[..close];
        let after = &inner[close + 1..];
        let port_text = after.strip_prefix(':').ok_or_else(rejected)?;
        (host, Some(port_text))
    } else if let Some(colon) = authority.rfind(':') {
        (&authority[..colon], Some(&authority[colon + 1..]))
    } else {
        (authority, None)
    };
    let port = match port {
        None => 80,
        Some("0") => return Err("target port cannot be zero".to_string()),
        Some(text) => text
            .parse::<u16>()
            .map_err(|_| format!("invalid port in {value:?}"))?,
    };
    let host = host_text
        .strip_suffix('.')
        .unwrap_or(host_text)
        .to_ascii_lowercase();
    if !is_loopback_host(&host) {
        return Err(rejected());
    }
    Ok(TargetUrl { host, port })
}

fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return inner
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

pub fn parse_config(args: &[String]) -> Result<ParseOutcome, String> {
    // `poros -- dashboard` still runs a program named dashboard.
    if args.first().map(String::as_str) == Some("dashboard") {
        if args.len() > 1 {
            return Err("dashboard takes no arguments".to_string());
        }
        return Ok(ParseOutcome::Dashboard);
    }
    let mut https_port: Option<u16> = None;
    let mut tailscale_cli: Option<String> = None;
    let mut target: Option<TargetUrl> = None;
    let mut version = false;
    let mut timeout = Duration::from_secs(30);
    let mut help = false;
    let mut parser = FlagParser::new(args);
    loop {
        match parser.next()? {
            Step::End | Step::Terminator => break,
            Step::Flag(name, value) => match name.as_str() {
                "https" => https_port = Some(parse_port(&value)?),
                "tailscale-cli" => tailscale_cli = Some(value),
                "target" => target = Some(parse_target(&value)?),
                "timeout" => {
                    timeout = parse_duration(&value)?;
                    if timeout.is_zero() {
                        return Err("timeout must be positive".to_string());
                    }
                }
                "version" => version = parse_flag_value(&value, &name)?,
                "help" | "h" => help = true,
                _ => return Err(format!("flag provided but not defined: -{name}")),
            },
        }
    }
    if help {
        return Ok(ParseOutcome::Help);
    }
    let command = parser.remaining();
    if version {
        return Ok(ParseOutcome::Version(env!("POROS_VERSION").to_string()));
    }
    if command.is_empty() && target.is_none() {
        return Err("provide a command or --target".to_string());
    }
    Ok(ParseOutcome::Config(Config {
        command,
        port: https_port.unwrap_or(0),
        tailscale_cli,
        target,
        version: false,
        timeout,
    }))
}

fn parse_port(value: &str) -> Result<u16, String> {
    let parsed: u32 = value
        .parse()
        .map_err(|_| "HTTPS port must be between 1 and 65535".to_string())?;
    u16::try_from(parsed).map_err(|_| "HTTPS port must be between 1 and 65535".to_string())
}

fn parse_flag_value(value: &str, name: &str) -> Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!("invalid boolean value for -{name}: {value:?}")),
    }
}

pub fn usage_text() -> String {
    let mut text = String::from("Usage: poros [options] command [args ...]\n");
    text.push_str("       poros dashboard\n");
    text.push_str("    \tlist listening servers and stop the ones you no longer need\n");
    text.push_str("Options:\n");
    text.push_str("  -https int\n    \tHTTPS port (default: an unused port)\n");
    text.push_str("  -tailscale-cli string\n    \tpath to Tailscale CLI\n");
    text.push_str("  -version\n    \tprint version\n");
    text.push_str("  -timeout duration\n    \tlocal server discovery deadline (default 30s)\n");
    text.push_str(
        "  -target string\n    \texplicit loopback HTTP URL instead of automatic discovery\n",
    );
    text
}
