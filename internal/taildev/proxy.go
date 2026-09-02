package taildev

import (
	"fmt"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
)

func newProxy(target *url.URL, node tailnetNode, stderr *log.Logger) http.Handler {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Proxy = nil
	proxy := &httputil.ReverseProxy{
		Transport: transport,
		Rewrite: func(request *httputil.ProxyRequest) {
			request.SetURL(target)
			request.Out.Host = target.Host
			request.SetXForwarded()
			if request.Out.Header.Get("Origin") != "" {
				request.Out.Header.Set("Origin", target.Scheme+"://"+target.Host)
			}
		},
		ErrorHandler: func(response http.ResponseWriter, _ *http.Request, err error) {
			stderr.Printf("backend unavailable: %v", err)
			http.Error(response, "Development server is not ready", http.StatusBadGateway)
		},
		ModifyResponse: func(response *http.Response) error {
			location := response.Header.Get("Location")
			if location == "" {
				return nil
			}
			parsedLocation, err := url.Parse(location)
			if err != nil || !parsedLocation.IsAbs() || !strings.EqualFold(parsedLocation.Host, target.Host) {
				return nil
			}
			parsedLocation.Scheme = response.Request.Header.Get("X-Forwarded-Proto")
			parsedLocation.Host = response.Request.Header.Get("X-Forwarded-Host")
			response.Header.Set("Location", parsedLocation.String())
			return nil
		},
	}

	allowedHosts := map[string]bool{node.ip.String(): true}
	if node.dnsName != "" {
		allowedHosts[strings.ToLower(node.dnsName)] = true
		if short, _, ok := strings.Cut(node.dnsName, "."); ok {
			allowedHosts[strings.ToLower(short)] = true
		}
	}

	return http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		host := request.Host
		if parsedHost, _, err := net.SplitHostPort(request.Host); err == nil {
			host = parsedHost
		}
		host = strings.ToLower(strings.Trim(host, "[]"))
		if !allowedHosts[host] {
			http.Error(response, "Host is not this Tailscale node", http.StatusForbidden)
			return
		}
		if origin := request.Header.Get("Origin"); origin != "" {
			parsedOrigin, err := url.Parse(origin)
			if err != nil || !strings.EqualFold(parsedOrigin.Host, request.Host) {
				http.Error(response, "Origin does not match this Tailscale node", http.StatusForbidden)
				return
			}
		}
		proxy.ServeHTTP(response, request)
	})
}

func newErrorLogger(output interface{ Write([]byte) (int, error) }) *log.Logger {
	return log.New(output, "taildev: ", 0)
}

func publicURL(node tailnetNode, port int) string {
	host := node.dnsName
	if host == "" {
		host = node.ip.String()
	}
	return fmt.Sprintf("http://%s/", net.JoinHostPort(host, fmt.Sprintf("%d", port)))
}
