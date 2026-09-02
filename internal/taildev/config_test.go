package taildev

import (
	"bytes"
	"net/url"
	"slices"
	"testing"
)

func TestParseConfigDefaults(t *testing.T) {
	configuration, err := parseConfig([]string{"--", "npm", "run", "dev"}, &bytes.Buffer{})
	if err != nil {
		t.Fatal(err)
	}
	if configuration.port != 5173 || configuration.target != nil {
		t.Fatalf("unexpected defaults: %#v", configuration)
	}
	if len(configuration.command) != 3 || configuration.command[0] != "npm" {
		t.Fatalf("unexpected command: %#v", configuration.command)
	}
}

func TestCommandEnvironmentUsesLocalTarget(t *testing.T) {
	configuration, err := parseConfig([]string{"--port", "8080", "--target", "http://127.0.0.1:3000"}, &bytes.Buffer{})
	if err != nil {
		t.Fatal(err)
	}
	environment := commandEnvironment(
		[]string{"PATH=/bin", "PORT=9000", "HOSTNAME=real-machine", "TAILDEV_URL=old"},
		configuration,
		"http://workstation.example.ts.net:8080/",
	)
	for _, expected := range []string{
		"HOST=127.0.0.1",
		"HOSTNAME=real-machine",
		"PATH=/bin",
		"PORT=3000",
		"TAILDEV_TARGET_HOST=127.0.0.1",
		"TAILDEV_URL=http://workstation.example.ts.net:8080/",
	} {
		if !slices.Contains(environment, expected) {
			t.Errorf("missing %q in %#v", expected, environment)
		}
	}
}

func TestParseConfigRejectsCredentials(t *testing.T) {
	_, err := parseConfig([]string{"--target", "http://user:password@localhost:3000"}, &bytes.Buffer{})
	if err == nil {
		t.Fatal("expected target credentials to be rejected")
	}
}

func TestParseConfigRejectsTargetPortZero(t *testing.T) {
	_, err := parseConfig([]string{"--target", "http://127.0.0.1:0"}, &bytes.Buffer{})
	if err == nil {
		t.Fatal("expected target port zero to be rejected")
	}
}

func TestExpandCommand(t *testing.T) {
	target, err := url.Parse("http://127.0.0.1:43123")
	if err != nil {
		t.Fatal(err)
	}
	expanded := expandCommand(
		[]string{"vite", "--host", "{host}", "--port={port}", "--public={url}"},
		target,
		"http://workstation.example.ts.net:5173/",
	)
	expected := []string{"vite", "--host", "127.0.0.1", "--port=43123", "--public=http://workstation.example.ts.net:5173/"}
	if !slices.Equal(expanded, expected) {
		t.Fatalf("unexpected command: %#v", expanded)
	}
}
