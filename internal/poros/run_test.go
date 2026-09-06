package poros

import (
	"bytes"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
)

func TestStopRemainingProcessGroupEscalates(t *testing.T) {
	child := exec.Command("sh", "-c", "trap '' TERM; sleep 60 & exit 0")
	child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	if err := child.Wait(); err != nil {
		t.Fatal(err)
	}

	stopRemainingProcessGroup(child, syscall.SIGTERM, time.Now().Add(100*time.Millisecond))
	if processGroupExists(child.Process.Pid) {
		t.Fatal("child process group still exists after cleanup")
	}
}

func fakeTailscale(t *testing.T) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "tailscale")
	script := `#!/bin/sh
case "$1" in
status) printf '%s\n' '{"BackendState":"Running","Self":{"DNSName":"test.ts.net.","TailscaleIPs":["100.100.100.100"]}}';;
serve) if [ "$2" = status ]; then printf '{}\n'; else exit 97; fi;;
*) exit 98;;
esac
`
	if err := os.WriteFile(path, []byte(script), 0700); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestRunPreservesEarlyChildExitAndEnvironment(t *testing.T) {
	t.Setenv("HOST", "unchanged")
	t.Setenv("PORT", "9876")
	cli := fakeTailscale(t)
	for _, status := range []struct {
		command string
		code    int
	}{
		{`test "$HOST" = unchanged && test "$PORT" = 9876 || exit 90; exit 23`, 23},
		{`kill -TERM $$`, 143},
		{`exit 0`, 0},
	} {
		var stderr lockedBuffer
		code := Run([]string{"--tailscale-cli", cli, "sh", "-c", status.command}, io.Discard, &stderr, "test")
		if code != status.code {
			t.Fatalf("got exit %d want %d: %s", code, status.code, stderr.String())
		}
	}
}

func TestRunDiscoveryTimeoutCleansChild(t *testing.T) {
	started := time.Now()
	var stderr lockedBuffer
	code := Run([]string{"--tailscale-cli", fakeTailscale(t), "--timeout", "200ms", "sh", "-c", "sleep 60"}, io.Discard, &stderr, "test")
	if code != 1 || time.Since(started) > 8*time.Second {
		t.Fatalf("unbounded timeout or wrong exit: %d %s", code, stderr.String())
	}
}

// Run streams child and proxy diagnostics concurrently, like an os.File.
type lockedBuffer struct {
	mu     sync.Mutex
	buffer bytes.Buffer
}

func (b *lockedBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buffer.Write(p)
}
func (b *lockedBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buffer.String()
}

func TestRunSignalHelper(t *testing.T) {
	if os.Getenv("POROS_TEST_SIGNAL_HELPER") != "1" {
		return
	}
	os.Exit(Run([]string{"--tailscale-cli", os.Getenv("POROS_TEST_CLI"), "--timeout", "1h", "sh", "-c", "sleep 60"}, os.Stdout, os.Stderr, "test"))
}

func TestSignalCancelsBlockedStartup(t *testing.T) {
	for _, stage := range []string{"status", "serve"} {
		t.Run(stage, func(t *testing.T) {
			dir := t.TempDir()
			marker := filepath.Join(dir, "started")
			cli := filepath.Join(dir, "tailscale")
			script := `#!/bin/sh
if [ "$1" = "$POROS_TEST_STAGE" ]; then
 : > "$POROS_TEST_MARKER"
 exec sleep 60
fi
printf '%s\n' '{"BackendState":"Running","Self":{"DNSName":"test.ts.net.","TailscaleIPs":["100.100.100.100"]}}'
`
			if err := os.WriteFile(cli, []byte(script), 0700); err != nil {
				t.Fatal(err)
			}
			child := exec.Command(os.Args[0], "-test.run=^TestRunSignalHelper$")
			child.Env = append(os.Environ(), "POROS_TEST_SIGNAL_HELPER=1", "POROS_TEST_CLI="+cli, "POROS_TEST_STAGE="+stage, "POROS_TEST_MARKER="+marker)
			child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
			if err := child.Start(); err != nil {
				t.Fatal(err)
			}
			defer signalProcessGroup(child, syscall.SIGKILL)
			done := make(chan error, 1)
			go func() { done <- child.Wait() }()
			ticker := time.NewTicker(20 * time.Millisecond)
			defer ticker.Stop()
			deadline := time.NewTimer(4 * time.Second)
			defer deadline.Stop()
			for {
				select {
				case <-deadline.C:
					t.Fatal("startup did not reach blocked command")
				case <-ticker.C:
					if _, err := os.Stat(marker); err != nil {
						continue
					}
					if err := child.Process.Signal(syscall.SIGHUP); err != nil {
						t.Fatal(err)
					}
					select {
					case err := <-done:
						if exitCode(err) != 129 {
							t.Fatalf("exit %d want 129", exitCode(err))
						}
						return
					case <-deadline.C:
						t.Fatal("signal did not cancel blocked startup")
					}
				}
			}
		})
	}
}

func TestServeRetriesOnlyTransientConflicts(t *testing.T) {
	for _, scenario := range []struct {
		mode     string
		attempts string
		code     int
	}{
		{"once", "2", 23}, {"denied", "1", 1}, {"always", "4", 1},
	} {
		t.Run(scenario.mode, func(t *testing.T) {
			dir := t.TempDir()
			counter, state := filepath.Join(dir, "attempts"), filepath.Join(dir, "route")
			t.Setenv("POROS_TEST_COUNTER", counter)
			t.Setenv("POROS_TEST_STATE", state)
			t.Setenv("POROS_TEST_MODE", scenario.mode)
			cli := filepath.Join(dir, "tailscale")
			script := `#!/bin/sh
if [ "$1" = status ]; then
 printf '%s\n' '{"BackendState":"Running","Self":{"DNSName":"test.ts.net.","TailscaleIPs":["100.100.100.100"]}}'
 exit 0
fi
if [ "$2" = status ]; then
 if [ -f "$POROS_TEST_STATE" ]; then cat "$POROS_TEST_STATE"; else printf '{}\n'; fi
 exit 0
fi
count=0
if [ -f "$POROS_TEST_COUNTER" ]; then read count < "$POROS_TEST_COUNTER"; fi
count=$((count+1))
printf '%s\n' "$count" > "$POROS_TEST_COUNTER"
if [ "$POROS_TEST_MODE" = denied ]; then echo 'access denied: configure operator' >&2; exit 1; fi
if [ "$POROS_TEST_MODE" = always ] || [ "$count" = 1 ]; then
 echo 'sending serve config: Preconditions failed: etag mismatch' >&2
 exit 1
fi
port=${3#--https=}
printf '{"Foreground":{"session":{"Web":{"test.ts.net:%s":{"Handlers":{"/":{"Proxy":"%s"}}}}}}}' "$port" "$4" > "$POROS_TEST_STATE"
echo 'CLI announcement should stay hidden'
trap 'rm -f "$POROS_TEST_STATE"; exit 0' INT TERM
sleep 60 & wait
`
			if err := os.WriteFile(cli, []byte(script), 0700); err != nil {
				t.Fatal(err)
			}
			var stdout, stderr lockedBuffer
			code := Run([]string{"--tailscale-cli", cli, "--timeout", "3s", "--target", "http://127.0.0.1:12345", "sh", "-c", `while [ ! -f "$POROS_TEST_STATE" ]; do sleep 0.02; done; sleep 0.4; exit 23`}, &stdout, &stderr, "test")
			if code != scenario.code {
				t.Fatalf("exit %d want %d: %s", code, scenario.code, stderr.String())
			}
			count, err := os.ReadFile(counter)
			if err != nil || strings.TrimSpace(string(count)) != scenario.attempts {
				t.Fatalf("attempts %s: %v", count, err)
			}
			if scenario.mode == "once" {
				if strings.Count(stdout.String(), "Poros URL:") != 1 || strings.Contains(stderr.String(), "etag") || strings.Contains(stdout.String()+stderr.String(), "CLI announcement") {
					t.Fatalf("wrong diagnostics: %s %s", stdout.String(), stderr.String())
				}
				if _, err := os.Stat(state); !os.IsNotExist(err) {
					t.Fatal("owned route not cleaned")
				}
			} else if stderr.String() == "" {
				t.Fatal("missing permanent failure details")
			}
		})
	}
}

func TestHelpSucceeds(t *testing.T) {
	var stderr lockedBuffer
	if code := Run([]string{"--help"}, io.Discard, &stderr, "test"); code != 0 {
		t.Fatalf("help exit %d", code)
	}
	if !strings.Contains(stderr.String(), "Usage:") || strings.Contains(stderr.String(), "help requested") {
		t.Fatalf("wrong help: %s", stderr.String())
	}
}
