package poros

import (
	"log"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
)

func newProxy(target *url.URL, authority string, stderr *log.Logger) http.Handler {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Proxy = nil
	proxy := &httputil.ReverseProxy{
		Transport: transport,
		Rewrite: func(request *httputil.ProxyRequest) {
			request.SetURL(target)
			request.Out.Host = target.Host
			request.SetXForwarded()
			request.Out.Header.Set("X-Forwarded-Proto", "https")
			request.Out.Header.Set("X-Forwarded-Host", authority)
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

	return http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if !strings.EqualFold(request.Host, authority) {
			http.Error(response, "Host is not this Poros route", http.StatusForbidden)
			return
		}
		if origin := request.Header.Get("Origin"); origin != "" {
			parsedOrigin, err := url.Parse(origin)
			if err != nil || parsedOrigin.Scheme != "https" || parsedOrigin.User != nil || parsedOrigin.Path != "" || parsedOrigin.RawQuery != "" || parsedOrigin.Fragment != "" || !strings.EqualFold(parsedOrigin.Host, authority) {
				http.Error(response, "Origin does not match this Tailscale node", http.StatusForbidden)
				return
			}
		}
		proxy.ServeHTTP(response, request)
	})
}

func newErrorLogger(output interface{ Write([]byte) (int, error) }) *log.Logger {
	return log.New(output, "poros: ", 0)
}
