package poros

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"os/exec"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Inspect ownership, not framework output or a guessed default port.
func discoverTarget(ctx context.Context, pid int) (*url.URL, error) {
	ctx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	ps, err := exec.CommandContext(ctx, "ps", "-axo", "pid=,ppid=,pgid=").Output()
	if err != nil {
		return nil, fmt.Errorf("inspect child processes: %w", err)
	}
	pids := ownedProcesses(string(ps), pid)
	if len(pids) == 0 {
		return nil, nil
	}
	output, err := exec.CommandContext(ctx, "lsof", "-nP", "-a", "-p", strings.Join(pids, ","), "-iTCP", "-sTCP:LISTEN", "-Fn").Output()
	if err != nil {
		if e, ok := err.(*exec.ExitError); !ok || e.ExitCode() != 1 {
			return nil, fmt.Errorf("inspect listening sockets (requires lsof): %w", err)
		}
	}
	addresses := loopbackListeners(string(output))
	transport := &http.Transport{Proxy: nil, DisableKeepAlives: true}
	defer transport.CloseIdleConnections()
	client := &http.Client{Transport: transport, Timeout: 400 * time.Millisecond, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	var targets []*url.URL
	for _, address := range addresses {
		u, _ := url.Parse("http://" + address)
		req, _ := http.NewRequestWithContext(ctx, http.MethodGet, u.String(), nil)
		resp, err := client.Do(req)
		if err == nil {
			resp.Body.Close()
			targets = append(targets, u)
		}
	}
	if len(targets) > 1 {
		return nil, fmt.Errorf("multiple child HTTP listeners found: %v; select one with --target", targets)
	}
	if len(targets) == 1 {
		return targets[0], nil
	}
	return nil, nil
}

// Only the process group we can terminate is supported. Detached servers are not owned.
func ownedProcesses(output string, root int) []string {
	result := []string{}
	for _, line := range strings.Split(output, "\n") {
		var pid, parent, group int
		if n, _ := fmt.Sscanf(line, "%d %d %d", &pid, &parent, &group); n == 3 && group == root {
			result = append(result, strconv.Itoa(pid))
		}
	}
	sort.Strings(result)
	return result
}

func loopbackListeners(output string) []string {
	seen := map[string]bool{}
	for _, line := range strings.Split(output, "\n") {
		if !strings.HasPrefix(line, "n") {
			continue
		}
		address := strings.TrimPrefix(line, "n")
		host, _, err := net.SplitHostPort(address)
		if err != nil {
			continue
		}
		if ip := net.ParseIP(host); ip != nil && ip.IsLoopback() {
			seen[address] = true
		}
	}
	result := make([]string, 0, len(seen))
	for address := range seen {
		result = append(result, address)
	}
	sort.Strings(result)
	return result
}
