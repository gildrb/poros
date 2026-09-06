package poros

import (
	"bytes"
	"reflect"
	"testing"
)

func TestCommandUnchanged(t *testing.T) {
	args := []string{"bun", "run", "dev", "--port", "5179"}
	c, err := parseConfig(args, &bytes.Buffer{})
	if err != nil {
		t.Fatal(err)
	}
	if c.target != nil || c.port != 0 || !reflect.DeepEqual(c.command, args) {
		t.Fatalf("unexpected config: %#v", c)
	}
}

func TestRejectInvalidConfig(t *testing.T) {
	for _, args := range [][]string{{}, {"--target", "http://user:pw@localhost:3000"}, {"--target", "http://127.0.0.1:0"}, {"--target", "http://example.com:3000"}, {"--target", "https://localhost:3000"}, {"--timeout", "0", "bun"}} {
		if _, err := parseConfig(args, &bytes.Buffer{}); err == nil {
			t.Fatalf("accepted %v", args)
		}
	}
}
