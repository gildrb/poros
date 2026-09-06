package poros

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"os/signal"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"time"
)

func Run(args []string, stdout, stderr io.Writer, version string) int {
	c, err := parseConfig(args, stderr)
	if errors.Is(err, flag.ErrHelp) {
		return 0
	}
	if err != nil {
		fmt.Fprintf(stderr, "poros: %v\n", err)
		return 2
	}
	if c.version {
		fmt.Fprintln(stdout, version)
		return 0
	}
	signalContext, cancelSignal := context.WithCancel(context.Background())
	defer cancelSignal()
	incoming := make(chan os.Signal, 1)
	signals := make(chan os.Signal, 1)
	var receivedSignal atomic.Int32
	signal.Notify(incoming, os.Interrupt, syscall.SIGTERM, syscall.SIGHUP)
	defer signal.Stop(incoming)
	go func() {
		select {
		case sig := <-incoming:
			receivedSignal.Store(int32(sig.(syscall.Signal)))
			cancelSignal()
			signals <- sig
		case <-signalContext.Done():
		}
	}()
	fail := func(err error) int {
		if sig := receivedSignal.Load(); sig != 0 {
			return 128 + int(sig)
		}
		fmt.Fprintf(stderr, "poros: %v\n", err)
		return 1
	}
	cli, err := findTailscaleCLI(c.tailscaleCLI)
	if err != nil {
		return fail(err)
	}
	node, err := loadTailnetNode(signalContext, cli)
	if err != nil {
		return fail(err)
	}
	if node.dnsName == "" {
		return fail(errors.New("Tailscale MagicDNS and HTTPS certificates are required"))
	}
	ctx, cancel := context.WithTimeout(signalContext, c.timeout)
	defer cancel()
	initial, err := readServeConfig(ctx, cli)
	if err != nil {
		return fail(err)
	}
	// Keep the bridge socket reserved: parallel Poros invocations choose distinct ports.
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return fail(err)
	}
	defer listener.Close()
	if c.port == 0 {
		c.port = listener.Addr().(*net.TCPAddr).Port
	}
	if c.port == 443 || c.port == 8443 || c.port == 10000 {
		return fail(errors.New("choose a dedicated HTTPS port, excluding Funnel ports 443, 8443, and 10000"))
	}
	if initial.usesPort(c.port) {
		return fail(fmt.Errorf("HTTPS port %d is already configured in Tailscale Serve; choose another with --https", c.port))
	}
	var child *exec.Cmd
	var childExit chan error
	if len(c.command) > 0 {
		if c.target == nil {
			for _, tool := range []string{"ps", "lsof"} {
				if _, err := exec.LookPath(tool); err != nil {
					return fail(fmt.Errorf("automatic discovery requires %s: %w", tool, err))
				}
			}
		}
		child = exec.Command(c.command[0], c.command[1:]...)
		child.Stdout, child.Stderr = stdout, stderr
		child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
		if err := child.Start(); err != nil {
			return fail(fmt.Errorf("start command: %w", err))
		}
		childExit = make(chan error, 1)
		go func() { childExit <- child.Wait() }()
		defer func() {
			if childExit != nil {
				sig := syscall.SIGTERM
				if received := receivedSignal.Load(); received != 0 {
					sig = syscall.Signal(received)
				}
				_ = terminateProcessGroup(child, childExit, sig)
			}
		}()
	}
	finishChild := func(err error) int {
		childExit = nil
		stopRemainingProcessGroup(child, syscall.SIGTERM, time.Now().Add(5*time.Second))
		return exitCode(err)
	}
	ticker := time.NewTicker(150 * time.Millisecond)
	defer ticker.Stop()
	for c.target == nil {
		select {
		case err := <-childExit:
			return finishChild(err)
		case sig := <-signals:
			err := terminateProcessGroup(child, childExit, sig)
			childExit = nil
			return exitCode(err)
		case <-ctx.Done():
			return fail(errors.New("no unique loopback HTTP listener found before timeout; bind to localhost or select --target"))
		case <-ticker.C:
			c.target, err = discoverTarget(ctx, child.Process.Pid)
			if err != nil {
				return fail(err)
			}
		}
	}
	authority := net.JoinHostPort(node.dnsName, strconv.Itoa(c.port))
	logger := newErrorLogger(stderr)
	server := &http.Server{Handler: newProxy(c.target, authority, logger), ErrorLog: logger, ReadHeaderTimeout: 10 * time.Second}
	defer server.Close()
	serverErrors := make(chan error, 1)
	go func() { serverErrors <- server.Serve(listener) }()
	bridge := "http://" + listener.Addr().String()
	var serve *exec.Cmd
	var serveExit chan error
	var serveOutput bytes.Buffer
	attempts := 0
	startServe := func() error {
		attempts++
		serveOutput.Reset()
		serve = exec.Command(cli, "serve", "--bg=false", "--https="+strconv.Itoa(c.port), bridge)
		serve.Stdout, serve.Stderr = &serveOutput, &serveOutput
		serve.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
		if err := serve.Start(); err != nil {
			return err
		}
		serveExit = make(chan error, 1)
		go func(command *exec.Cmd, exited chan<- error) { exited <- command.Wait() }(serve, serveExit)
		return nil
	}
	if err := startServe(); err != nil {
		return fail(fmt.Errorf("start Tailscale Serve: %w", err))
	}
	ready := false
	defer func() {
		if serveExit != nil {
			_ = terminateProcessGroup(serve, serveExit, os.Interrupt)
		}
		// Wait completed before reading captured output. Normal CLI route announcements
		// and recoverable conflicts stay hidden; only Poros prints the ready URL.
		if !ready && receivedSignal.Load() == 0 {
			fmt.Fprint(stderr, serveOutput.String())
		}
	}()
	var retry <-chan time.Time
	for {
		select {
		case err := <-childExit:
			return finishChild(err)
		case <-retry:
			retry = nil
			if err := startServe(); err != nil {
				return fail(fmt.Errorf("restart Tailscale Serve: %w", err))
			}
		case err := <-serveExit:
			serveExit = nil
			if !ready && err != nil && attempts < 4 && ctx.Err() == nil && strings.Contains(serveOutput.String(), "etag mismatch") {
				timer := time.NewTimer(time.Duration(attempts) * 100 * time.Millisecond)
				defer timer.Stop()
				retry = timer.C
				continue
			}
			return fail(fmt.Errorf("Tailscale Serve stopped (%v); check HTTPS setup, permissions, and port conflicts", err))
		case err := <-serverErrors:
			return fail(fmt.Errorf("local bridge stopped: %w", err))
		case sig := <-signals:
			if child == nil {
				return 128 + int(sig.(syscall.Signal))
			}
			err := terminateProcessGroup(child, childExit, sig)
			childExit = nil
			return exitCode(err)
		case <-ticker.C:
			if ready {
				continue
			}
			if ctx.Err() != nil {
				return fail(errors.New("Tailscale Serve startup timed out; check HTTPS setup and permissions"))
			}
			if serveExit == nil {
				continue
			}
			status, err := readServeConfig(ctx, cli)
			if err != nil {
				return fail(err)
			}
			if status.ownsRoute(authority, bridge) {
				ready = true
				fmt.Fprintf(stdout, "Poros URL: https://%s/\nLocal target: %s\n", authority, c.target)
			}
		}
	}
}

func signalProcessGroup(child *exec.Cmd, signal syscall.Signal) error {
	return syscall.Kill(-child.Process.Pid, signal)
}

func terminateProcessGroup(child *exec.Cmd, childExit <-chan error, received os.Signal) error {
	signal, ok := received.(syscall.Signal)
	if !ok {
		signal = syscall.SIGTERM
	}
	deadline := time.Now().Add(5 * time.Second)
	_ = signalProcessGroup(child, signal)
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	select {
	case err := <-childExit:
		stopRemainingProcessGroup(child, signal, deadline)
		return err
	case <-timer.C:
		_ = signalProcessGroup(child, syscall.SIGKILL)
		err := <-childExit
		waitForProcessGroupExit(child.Process.Pid, time.Now().Add(time.Second))
		return err
	}
}

func stopRemainingProcessGroup(child *exec.Cmd, signal syscall.Signal, deadline time.Time) {
	_ = signalProcessGroup(child, signal)
	waitForProcessGroupExit(child.Process.Pid, deadline)
	if processGroupExists(child.Process.Pid) {
		_ = signalProcessGroup(child, syscall.SIGKILL)
		waitForProcessGroupExit(child.Process.Pid, time.Now().Add(time.Second))
	}
}

func waitForProcessGroupExit(processGroupID int, deadline time.Time) {
	for processGroupExists(processGroupID) && time.Now().Before(deadline) {
		time.Sleep(25 * time.Millisecond)
	}
}

func processGroupExists(processGroupID int) bool {
	err := syscall.Kill(-processGroupID, 0)
	return err == nil || errors.Is(err, syscall.EPERM)
}

func exitCode(err error) int {
	if err == nil {
		return 0
	}
	var exitError *exec.ExitError
	if errors.As(err, &exitError) {
		if status, ok := exitError.Sys().(syscall.WaitStatus); ok && status.Signaled() {
			return 128 + int(status.Signal())
		}
		return exitError.ExitCode()
	}
	return 1
}
