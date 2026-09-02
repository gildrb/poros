package taildev

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/url"
	"strconv"
	"strings"
)

const defaultPort = 5173

type config struct {
	command      []string
	port         int
	tailscaleCLI string
	target       *url.URL
	version      bool
}

func parseConfig(args []string, stderr io.Writer) (config, error) {
	flags := flag.NewFlagSet("taildev", flag.ContinueOnError)
	flags.SetOutput(stderr)
	port := flags.Int("port", defaultPort, "tailnet port to expose")
	target := flags.String("target", "", "local HTTP server URL (default: automatic port for wrapped commands)")
	tailscaleCLI := flags.String("tailscale-cli", "", "path to the Tailscale CLI")
	showVersion := flags.Bool("version", false, "print the version")
	flags.Usage = func() {
		fmt.Fprintln(stderr, "Usage: taildev [options] [-- command ...]")
		fmt.Fprintln(stderr, "Expose a localhost development server only to your tailnet.")
		flags.PrintDefaults()
	}

	if err := flags.Parse(args); err != nil {
		return config{}, err
	}
	if *showVersion {
		return config{version: true}, nil
	}
	if *port < 1 || *port > 65_535 {
		return config{}, fmt.Errorf("port must be between 1 and 65535: %d", *port)
	}

	targetValue := strings.TrimSpace(*target)
	if targetValue == "" {
		return config{
			command:      flags.Args(),
			port:         *port,
			tailscaleCLI: strings.TrimSpace(*tailscaleCLI),
		}, nil
	}
	targetURL, err := url.Parse(targetValue)
	if err != nil {
		return config{}, fmt.Errorf("parse target: %w", err)
	}
	if targetURL.Scheme != "http" && targetURL.Scheme != "https" {
		return config{}, errors.New("target must use http or https")
	}
	if targetURL.Hostname() == "" {
		return config{}, errors.New("target must include a hostname")
	}
	if targetURL.User != nil {
		return config{}, errors.New("target credentials are not supported")
	}
	if targetURL.Port() == "" {
		defaultTargetPort := "80"
		if targetURL.Scheme == "https" {
			defaultTargetPort = "443"
		}
		targetURL.Host = net.JoinHostPort(targetURL.Hostname(), defaultTargetPort)
	}
	targetPort, err := strconv.ParseUint(targetURL.Port(), 10, 16)
	if err != nil || targetPort == 0 {
		return config{}, fmt.Errorf("invalid target port %q", targetURL.Port())
	}

	return config{
		command:      flags.Args(),
		port:         *port,
		tailscaleCLI: strings.TrimSpace(*tailscaleCLI),
		target:       targetURL,
	}, nil
}
