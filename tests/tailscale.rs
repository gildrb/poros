use poros::tailscale::{parse_node, parse_serve_config};

#[test]
fn parses_running_node() {
    let document = r#"{
		"BackendState": "Running",
		"Self": { "DNSName": "server.tail1234.ts.net." }
	}"#;
    let node = parse_node(document).expect("parses");
    assert_eq!(node.dns_name, "server.tail1234.ts.net");
}

#[test]
fn rejects_stopped_backend() {
    let document = r#"{ "BackendState": "NeedsLogin", "Self": { "DNSName": "x.ts.net." } }"#;
    let error = parse_node(document).expect_err("rejected");
    assert!(error.contains("not running"));
}

#[test]
fn rejects_missing_dns_name() {
    let document = r#"{ "BackendState": "Running", "Self": { "DNSName": "" } }"#;
    let error = parse_node(document).expect_err("rejected");
    assert!(error.contains("MagicDNS"));
}

#[test]
fn empty_serve_status_is_default() {
    let config = parse_serve_config("null").expect("parses");
    assert!(!config.uses_port(8080));
    assert!(!config.owns_route("x.ts.net:8080", "http://127.0.0.1:1"));
}

#[test]
fn serve_config_unknown_fields_rejected() {
    assert!(parse_serve_config(r#"{"Unknown": true}"#).is_err());
}

#[test]
fn uses_port_checks_all_sections() {
    let document = r#"{
		"TCP": { "8080": {} },
		"Web": { "node.ts.net:9090": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:1" } } } },
		"AllowFunnel": { "node.ts.net:7070": true, "node.ts.net:7071": false },
		"Foreground": {
			"abc": {
				"TCP": {},
				"Web": { "node.ts.net:6060": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:2" } } } },
				"AllowFunnel": {},
				"Services": null
			}
		}
	}"#;
    let config = parse_serve_config(document).expect("parses");
    assert!(config.uses_port(8080));
    assert!(config.uses_port(9090));
    assert!(config.uses_port(7070));
    assert!(!config.uses_port(7071), "funnel disabled on that port");
    assert!(config.uses_port(6060));
    assert!(!config.uses_port(5050));
}

#[test]
fn owns_route_requires_exact_proxy_and_no_funnel() {
    let document = r#"{
		"Foreground": {
			"child": {
				"TCP": {},
				"Web": {
					"node.ts.net:9119": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:4567" } } }
				},
				"AllowFunnel": {},
				"Services": null
			}
		}
	}"#;
    let config = parse_serve_config(document).expect("parses");
    assert!(config.owns_route("node.ts.net:9119", "http://127.0.0.1:4567"));
    assert!(!config.owns_route("node.ts.net:9119", "http://127.0.0.1:9999"));
    assert!(!config.owns_route("other.ts.net:9119", "http://127.0.0.1:4567"));
}

#[test]
fn background_routes_are_not_owned() {
    let document = r#"{
		"TCP": {},
		"Web": { "node.ts.net:9119": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:4567" } } } }
	}"#;
    let config = parse_serve_config(document).expect("parses");
    assert!(!config.owns_route("node.ts.net:9119", "http://127.0.0.1:4567"));
}
