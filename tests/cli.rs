use poros::cli::{parse_config, usage_text, ParseOutcome};

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

#[test]
fn parses_command_with_flags() {
    let outcome = parse_config(&args(&["-https", "9119", "vp", "dev"])).expect("parses");
    let ParseOutcome::Config(config) = outcome else {
        panic!("expected config");
    };
    assert_eq!(config.port, 9119);
    assert_eq!(config.command, vec!["vp", "dev"]);
    assert!(config.target.is_none());
}

#[test]
fn parses_equals_form_and_auto_port() {
    let outcome = parse_config(&args(&["--https=8123", "bun", "run", "dev"])).expect("parses");
    let ParseOutcome::Config(config) = outcome else {
        panic!("expected config");
    };
    assert_eq!(config.port, 8123);
    assert_eq!(config.command, vec!["bun", "run", "dev"]);
}

#[test]
fn bool_flags_work_bare() {
    let outcome = parse_config(&args(&["--version"])).expect("parses");
    assert!(matches!(outcome, ParseOutcome::Version(_)));
    let outcome = parse_config(&args(&["--version=false", "sh"])).expect("parses");
    let ParseOutcome::Config(config) = outcome else {
        panic!("expected config");
    };
    assert_eq!(config.command, vec!["sh"]);
}

#[test]
fn double_dash_separates_command() {
    let outcome = parse_config(&args(&["--", "-weird", "arg"])).expect("parses");
    let ParseOutcome::Config(config) = outcome else {
        panic!("expected config");
    };
    assert_eq!(config.command, vec!["-weird", "arg"]);
}

#[test]
fn rejects_missing_command_and_target() {
    let error = parse_config(&args(&[])).expect_err("empty args rejected");
    assert_eq!(error, "provide a command or --target");
}

#[test]
fn rejects_unknown_flag() {
    let error = parse_config(&args(&["-nope", "sh"])).expect_err("unknown flag rejected");
    assert_eq!(error, "flag provided but not defined: -nope");
}

#[test]
fn rejects_out_of_range_https_port() {
    let error = parse_config(&args(&["-https", "70000", "sh"])).expect_err("port rejected");
    assert!(error.contains("between 1 and 65535"));
}

#[test]
fn parses_duration_units() {
    let outcome = parse_config(&args(&["-timeout", "45s", "sh"])).expect("parses");
    let ParseOutcome::Config(config) = outcome else {
        panic!("expected config");
    };
    assert_eq!(config.timeout, std::time::Duration::from_secs(45));
    let error = parse_config(&args(&["-timeout", "0s", "sh"])).expect_err("zero rejected");
    assert!(error.contains("positive"));
}

#[test]
fn target_accepts_loopback_urls() {
    for value in [
        "http://127.0.0.1:3000",
        "http://localhost:3000",
        "http://[::1]:3000",
        "http://127.0.0.1",
    ] {
        let outcome = parse_config(&args(&["-target", value])).expect(value);
        let ParseOutcome::Config(config) = outcome else {
            panic!("expected config for {value}");
        };
        assert!(config.target.is_some(), "{value}");
        assert!(config.command.is_empty(), "{value}");
    }
}

#[test]
fn target_rejects_non_loopback_and_decorations() {
    for value in [
        "http://example.com:3000",
        "https://127.0.0.1:3000",
        "http://user@127.0.0.1:3000",
        "http://127.0.0.1:3000/path",
        "http://127.0.0.1:3000?q=1",
        "http://127.0.0.1:0",
        "127.0.0.1:3000",
    ] {
        assert!(
            parse_config(&args(&["-target", value])).is_err(),
            "{value} should be rejected"
        );
    }
}

#[test]
fn prints_usage() {
    assert!(usage_text().starts_with("Usage: poros [options] command [args ...]"));
}
