package taildev

import (
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"
)

func Run(args []string, stdout, stderr io.Writer, version string) int {
	configuration, err := parseConfig(args, stderr)
	if err != nil {
		return 2
	}
	if configuration.version {
		fmt.Fprintln(stdout, version)
		return 0
	}

	tailscaleCLI, err := findTailscaleCLI(configuration.tailscaleCLI)
	if err != nil {
		fmt.Fprintf(stderr, "taildev: %v\n", err)
		return 1
	}
	node, err := loadTailnetNode(tailscaleCLI)
	if err != nil {
		fmt.Fprintf(stderr, "taildev: %v\n", err)
		return 1
	}
	configuration.target, err = resolveTarget(configuration)
	if err != nil {
		fmt.Fprintf(stderr, "taildev: select local target: %v\n", err)
		return 1
	}

	listener, err := net.Listen("tcp", net.JoinHostPort(node.ip.String(), strconv.Itoa(configuration.port)))
	if err != nil {
		fmt.Fprintf(stderr, "taildev: listen on %s: %v\n", node.ip, err)
		fmt.Fprintln(stderr, "taildev: ensure the development server listens on localhost, not all interfaces")
		return 1
	}

	logger := newErrorLogger(stderr)
	server := &http.Server{
		Handler:           newProxy(configuration.target, node, logger),
		ErrorLog:          logger,
		ReadHeaderTimeout: 10 * time.Second,
	}
	serverErrors := make(chan error, 1)
	go func() {
		err := server.Serve(listener)
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			serverErrors <- err
		}
	}()

	url := publicURL(node, configuration.port)
	fmt.Fprintf(stdout, "Tailnet URL: %s\n", url)
	fmt.Fprintf(stdout, "Local target: %s\n", configuration.target)

	signals := make(chan os.Signal, 1)
	signal.Notify(signals, os.Interrupt, syscall.SIGTERM)
	defer signal.Stop(signals)

	if len(configuration.command) == 0 {
		select {
		case <-signals:
			_ = server.Close()
			return 0
		case err := <-serverErrors:
			fmt.Fprintf(stderr, "taildev: proxy failed: %v\n", err)
			return 1
		}
	}

	command := expandCommand(configuration.command, configuration.target, url)
	child := exec.Command(command[0], command[1:]...)
	child.Stdout = stdout
	child.Stderr = stderr
	child.Env = commandEnvironment(os.Environ(), configuration, url)
	child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := child.Start(); err != nil {
		_ = server.Close()
		fmt.Fprintf(stderr, "taildev: start command: %v\n", err)
		return 1
	}

	childExit := make(chan error, 1)
	go func() { childExit <- child.Wait() }()

	select {
	case err := <-childExit:
		stopRemainingProcessGroup(child, syscall.SIGTERM, time.Now().Add(5*time.Second))
		_ = server.Close()
		return exitCode(err)
	case err := <-serverErrors:
		_ = terminateProcessGroup(child, childExit, os.Interrupt)
		_ = server.Close()
		fmt.Fprintf(stderr, "taildev: proxy failed: %v\n", err)
		return 1
	case received := <-signals:
		err := terminateProcessGroup(child, childExit, received)
		_ = server.Close()
		return exitCode(err)
	}
}

func resolveTarget(configuration config) (*url.URL, error) {
	if configuration.target != nil {
		return configuration.target, nil
	}
	if len(configuration.command) == 0 {
		return url.Parse(fmt.Sprintf("http://127.0.0.1:%d", configuration.port))
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	address := listener.Addr().String()
	if err := listener.Close(); err != nil {
		return nil, err
	}
	return url.Parse("http://" + address)
}

func expandCommand(command []string, target *url.URL, publicURL string) []string {
	replacer := strings.NewReplacer(
		"{host}", target.Hostname(),
		"{port}", target.Port(),
		"{url}", publicURL,
	)
	expanded := make([]string, len(command))
	for index, argument := range command {
		expanded[index] = replacer.Replace(argument)
	}
	return expanded
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

func commandEnvironment(current []string, configuration config, publicURL string) []string {
	values := map[string]string{
		"HOST":                configuration.target.Hostname(),
		"PORT":                configuration.target.Port(),
		"TAILDEV_TARGET_HOST": configuration.target.Hostname(),
		"TAILDEV_URL":         publicURL,
	}
	result := make([]string, 0, len(current)+len(values))
	for _, item := range current {
		name, _, found := strings.Cut(item, "=")
		if !found {
			continue
		}
		if _, replaced := values[name]; !replaced {
			result = append(result, item)
		}
	}
	for name, value := range values {
		result = append(result, name+"="+value)
	}
	return result
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
