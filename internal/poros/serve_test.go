package poros

import "testing"

func TestServeConfigIsolation(t *testing.T) {
	c, err := parseServeConfig([]byte(`{"TCP":{"443":{"HTTPS":true}},"Foreground":{"other":{"TCP":{"5000":{"HTTPS":true}}},"ours":{"TCP":{"5001":{"HTTPS":true}},"Web":{"host.ts.net:5001":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:1234"}}}}}}}`))
	if err != nil {
		t.Fatal(err)
	}
	if !c.usesPort(443) || !c.usesPort(5000) || c.usesPort(5002) {
		t.Fatal("wrong occupied ports")
	}
	if !c.ownsRoute("host.ts.net:5001", "http://127.0.0.1:1234") || c.ownsRoute("host.ts.net:5001", "http://127.0.0.1:9999") {
		t.Fatal("wrong route ownership")
	}
	c.AllowFunnel = map[string]bool{"host.ts.net:5001": true}
	if c.ownsRoute("host.ts.net:5001", "http://127.0.0.1:1234") {
		t.Fatal("accepted public route")
	}
}

func TestServeStatusFailsClosed(t *testing.T) {
	for _, input := range []string{`null`, `broken`, `{"UnknownRoutingField":{}}`} {
		if _, err := parseServeConfig([]byte(input)); err == nil {
			t.Fatalf("accepted %s", input)
		}
	}
}

func TestServeSupportsUnrelatedCurrentHandlers(t *testing.T) {
	c, err := parseServeConfig([]byte(`{"Web":{"other.ts.net:443":{"Handlers":{"/":{"Proxy":"http://localhost:8000","AcceptAppCaps":["example.com/cap"],"Redirect":"https://elsewhere.example"}}}}}`))
	if err != nil {
		t.Fatal(err)
	}
	if !c.usesPort(443) || c.usesPort(55000) {
		t.Fatal("wrong port selection")
	}
}
