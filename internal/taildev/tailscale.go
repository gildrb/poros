package taildev

import (
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"runtime"
	"strings"
)

type tailnetNode struct {
	dnsName string
	ip      net.IP
}

type tailscaleStatus struct {
	BackendState string `json:"BackendState"`
	Self         struct {
		DNSName      string   `json:"DNSName"`
		TailscaleIPs []string `json:"TailscaleIPs"`
	} `json:"Self"`
}

func findTailscaleCLI(explicit string) (string, error) {
	candidates := []string{explicit, os.Getenv("TAILSCALE_CLI")}
	if runtime.GOOS == "darwin" {
		candidates = append(candidates, "/Applications/Tailscale.app/Contents/MacOS/Tailscale")
	}
	for _, candidate := range candidates {
		if candidate == "" {
			continue
		}
		info, err := os.Stat(candidate)
		if err == nil && !info.IsDir() && info.Mode()&0o111 != 0 {
			return candidate, nil
		}
	}
	if path, err := exec.LookPath("tailscale"); err == nil {
		return path, nil
	}
	return "", errors.New("Tailscale CLI not found; install Tailscale or set TAILSCALE_CLI")
}

func loadTailnetNode(cli string) (tailnetNode, error) {
	command := exec.Command(cli, "status", "--json", "--peers=false")
	output, err := command.Output()
	if err != nil {
		return tailnetNode{}, fmt.Errorf("read Tailscale status: %w", err)
	}
	return parseTailnetNode(output)
}

func parseTailnetNode(data []byte) (tailnetNode, error) {
	var status tailscaleStatus
	if err := json.Unmarshal(data, &status); err != nil {
		return tailnetNode{}, fmt.Errorf("parse Tailscale status: %w", err)
	}
	if status.BackendState != "Running" {
		return tailnetNode{}, fmt.Errorf("Tailscale is not running (state: %s)", status.BackendState)
	}

	var selectedIP net.IP
	for _, value := range status.Self.TailscaleIPs {
		candidate := net.ParseIP(value)
		if candidate != nil && candidate.To4() != nil {
			selectedIP = candidate.To4()
			break
		}
		if selectedIP == nil && candidate != nil {
			selectedIP = candidate
		}
	}
	if selectedIP == nil {
		return tailnetNode{}, errors.New("this node has no Tailscale IP")
	}

	return tailnetNode{
		dnsName: strings.TrimSuffix(status.Self.DNSName, "."),
		ip:      selectedIP,
	}, nil
}
