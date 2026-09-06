package poros

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"os/exec"
	"strconv"
	"time"
)

type serveConfig struct {
	TCP map[string]json.RawMessage
	Web map[string]struct {
		Handlers map[string]struct {
			Proxy         string
			Path          string
			Text          string
			AcceptAppCaps json.RawMessage
			Redirect      string
		}
	}
	Foreground  map[string]*serveConfig
	AllowFunnel map[string]bool
	Services    json.RawMessage
	ETag        string
}

func readServeConfig(ctx context.Context, cli string) (*serveConfig, error) {
	ctx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	output, err := exec.CommandContext(ctx, cli, "serve", "status", "--json").Output()
	if err != nil {
		return nil, fmt.Errorf("read Serve status: %w", err)
	}
	return parseServeConfig(output)
}

func parseServeConfig(output []byte) (*serveConfig, error) {
	var c *serveConfig
	dec := json.NewDecoder(bytes.NewReader(output))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&c); err != nil {
		return nil, fmt.Errorf("unsupported Serve status: %w", err)
	}
	if c == nil {
		return nil, fmt.Errorf("unsupported null Serve status")
	}
	return c, nil
}

func (c *serveConfig) usesPort(port int) bool {
	if _, ok := c.TCP[strconv.Itoa(port)]; ok {
		return true
	}
	for host := range c.Web {
		_, p, _ := net.SplitHostPort(host)
		if p == strconv.Itoa(port) {
			return true
		}
	}
	for host, enabled := range c.AllowFunnel {
		_, p, _ := net.SplitHostPort(host)
		if enabled && p == strconv.Itoa(port) {
			return true
		}
	}
	for _, child := range c.Foreground {
		if child == nil || child.usesPort(port) {
			return true
		}
	}
	return false
}

func (c *serveConfig) ownsRoute(host, target string) bool {
	for _, child := range c.Foreground {
		if child == nil {
			continue
		}
		route, ok := child.Web[host]
		if ok && route.Handlers["/"].Proxy == target && !child.AllowFunnel[host] && !c.AllowFunnel[host] {
			return true
		}
	}
	return false
}
