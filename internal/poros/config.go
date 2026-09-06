package poros

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/url"
	"strings"
	"time"
)

type config struct {
	command      []string
	port         int
	tailscaleCLI string
	target       *url.URL
	version      bool
	timeout      time.Duration
}

func parseConfig(args []string, stderr io.Writer) (config, error) {
	var c config
	flags := flag.NewFlagSet("poros", flag.ContinueOnError)
	flags.SetOutput(stderr)
	flags.IntVar(&c.port, "https", 0, "HTTPS port (default: an unused port)")
	flags.StringVar(&c.tailscaleCLI, "tailscale-cli", "", "path to Tailscale CLI")
	flags.BoolVar(&c.version, "version", false, "print version")
	flags.DurationVar(&c.timeout, "timeout", 30*time.Second, "local server discovery deadline")
	target := flags.String("target", "", "explicit loopback HTTP URL instead of automatic discovery")
	flags.Usage = func() {
		fmt.Fprintln(stderr, "Usage: poros [options] command [args ...]")
		flags.PrintDefaults()
	}
	if err := flags.Parse(args); err != nil {
		return c, err
	}
	c.command = flags.Args()
	if c.version {
		return c, nil
	}
	if c.port < 0 || c.port > 65535 {
		return c, errors.New("HTTPS port must be between 1 and 65535")
	}
	if c.timeout <= 0 {
		return c, errors.New("timeout must be positive")
	}
	if *target != "" {
		u, err := url.Parse(*target)
		if err != nil {
			return c, err
		}
		ip := net.ParseIP(u.Hostname())
		if u.Scheme != "http" || u.User != nil || u.RawQuery != "" || u.Fragment != "" || (u.Path != "" && u.Path != "/") || (u.Hostname() != "localhost" && (ip == nil || !ip.IsLoopback())) {
			return c, errors.New("target must be a loopback HTTP URL without credentials, path, query, or fragment")
		}
		if _, err := net.LookupPort("tcp", u.Port()); u.Port() != "" && err != nil {
			return c, err
		}
		if u.Port() == "0" {
			return c, errors.New("target port cannot be zero")
		}
		c.target = u
	}
	if len(c.command) == 0 && c.target == nil {
		return c, errors.New("provide a command or --target")
	}
	c.tailscaleCLI = strings.TrimSpace(c.tailscaleCLI)
	return c, nil
}
