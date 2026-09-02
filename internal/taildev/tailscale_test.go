package taildev

import "testing"

func TestParseTailnetNode(t *testing.T) {
	node, err := parseTailnetNode([]byte(`{
  "BackendState": "Running",
  "Self": {
    "DNSName": "workstation.example.ts.net.",
    "TailscaleIPs": ["100.100.100.100", "fd7a:115c:a1e0::1"]
  }
}`))
	if err != nil {
		t.Fatal(err)
	}
	if node.dnsName != "workstation.example.ts.net" {
		t.Fatalf("unexpected DNS name: %q", node.dnsName)
	}
	if node.ip.String() != "100.100.100.100" {
		t.Fatalf("unexpected IP: %s", node.ip)
	}
}

func TestParseTailnetNodeRequiresRunningState(t *testing.T) {
	_, err := parseTailnetNode([]byte(`{"BackendState":"NeedsLogin","Self":{}}`))
	if err == nil {
		t.Fatal("expected non-running state to fail")
	}
}
