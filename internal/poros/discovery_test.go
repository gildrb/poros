package poros

import (
	"context"
	"net"
	"net/http"
	"os"
	"os/exec"
	"reflect"
	"syscall"
	"testing"
	"time"
)

func TestOwnedProcessesIncludesGroupButNotDetachedDescendants(t *testing.T) {
	got := ownedProcesses("13 12 1\n12 11 1\n11 10 10\n10 1 10\n14 1 10\n99 1 99\n", 10)
	want := []string{"10", "11", "14"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v want %v", got, want)
	}
}

func TestLoopbackListeners(t *testing.T) {
	got := loopbackListeners("p123\nn[::1]:5174\nn127.0.0.1:3000\nn*:4000\nn100.1.2.3:5000\nn127.0.0.1:3000\n")
	want := []string{"127.0.0.1:3000", "[::1]:5174"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v want %v", got, want)
	}
}

func TestDiscoveryHelper(t *testing.T) {
	if os.Getenv("POROS_TEST_LISTEN") != "1" {
		return
	}
	listener, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		os.Exit(2)
	}
	_ = http.Serve(listener, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(204) }))
	os.Exit(0)
}

func TestDiscoverDescendantIPv6HTTP(t *testing.T) {
	if os.Getenv("POROS_TEST_NO_PROCESS_INSPECTION") == "1" {
		t.Skip("Darwin Nix sandbox forbids executing system ps; covered by native go test")
	}
	if _, err := exec.LookPath("lsof"); err != nil {
		t.Fatal(err)
	}
	child := exec.Command("sh", "-c", `"$1" -test.run=^TestDiscoveryHelper$ & wait`, "poros-test", os.Args[0])
	child.Env = append(os.Environ(), "POROS_TEST_LISTEN=1")
	child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := child.Start(); err != nil {
		t.Fatal(err)
	}
	exited := make(chan error, 1)
	go func() { exited <- child.Wait() }()
	defer terminateProcessGroup(child, exited, syscall.SIGTERM)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	ticker := time.NewTicker(50 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			t.Fatal("IPv6 child listener not discovered")
		case <-ticker.C:
			target, err := discoverTarget(ctx, child.Process.Pid)
			if err != nil {
				t.Fatal(err)
			}
			if target != nil {
				if target.Hostname() != "::1" {
					t.Fatalf("wrong target %s", target)
				}
				return
			}
		}
	}
}
