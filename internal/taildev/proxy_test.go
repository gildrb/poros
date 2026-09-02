package taildev

import (
	"bufio"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"
)

func TestProxyRewritesHostAndOrigin(t *testing.T) {
	var expectedBackendURL string
	backend := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.Host != strings.TrimPrefix(expectedBackendURL, "http://") {
			t.Errorf("unexpected backend Host: %q", request.Host)
		}
		if request.Header.Get("Origin") != expectedBackendURL {
			t.Errorf("unexpected backend Origin: %q", request.Header.Get("Origin"))
		}
		response.WriteHeader(http.StatusNoContent)
	}))
	defer backend.Close()
	expectedBackendURL = backend.URL

	target, err := url.Parse(backend.URL)
	if err != nil {
		t.Fatal(err)
	}
	node := tailnetNode{dnsName: "workstation.example.ts.net", ip: net.ParseIP("100.100.100.100")}
	proxy := httptest.NewServer(newProxy(target, node, log.New(io.Discard, "", 0)))
	defer proxy.Close()

	request, err := http.NewRequest(http.MethodGet, proxy.URL, nil)
	if err != nil {
		t.Fatal(err)
	}
	request.Host = "workstation.example.ts.net:5173"
	request.Header.Set("Origin", "http://workstation.example.ts.net:5173")
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusNoContent {
		t.Fatalf("unexpected status: %d", response.StatusCode)
	}
}

func TestProxyRejectsOtherHosts(t *testing.T) {
	target, _ := url.Parse("http://127.0.0.1:3000")
	node := tailnetNode{dnsName: "workstation.example.ts.net", ip: net.ParseIP("100.100.100.100")}
	request := httptest.NewRequest(http.MethodGet, "http://attacker.example/", nil)
	response := httptest.NewRecorder()

	newProxy(target, node, log.New(io.Discard, "", 0)).ServeHTTP(response, request)
	if response.Code != http.StatusForbidden {
		t.Fatalf("unexpected status: %d", response.Code)
	}
}

func TestProxyRejectsForeignOrigin(t *testing.T) {
	backendCalled := false
	backend := httptest.NewServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		backendCalled = true
	}))
	defer backend.Close()
	target, _ := url.Parse(backend.URL)
	node := tailnetNode{dnsName: "workstation.example.ts.net", ip: net.ParseIP("100.100.100.100")}
	request := httptest.NewRequest(http.MethodPost, "http://workstation.example.ts.net:5173/save", nil)
	request.Header.Set("Origin", "https://malicious.example")
	response := httptest.NewRecorder()

	newProxy(target, node, log.New(io.Discard, "", 0)).ServeHTTP(response, request)
	if response.Code != http.StatusForbidden {
		t.Fatalf("unexpected status: %d", response.Code)
	}
	if backendCalled {
		t.Fatal("foreign-origin request reached backend")
	}
}

func TestProxyRewritesBackendRedirect(t *testing.T) {
	var backendURL string
	backend := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, _ *http.Request) {
		response.Header().Set("Location", backendURL+"/login")
		response.WriteHeader(http.StatusTemporaryRedirect)
	}))
	defer backend.Close()
	backendURL = backend.URL
	target, _ := url.Parse(backend.URL)
	node := tailnetNode{dnsName: "workstation.example.ts.net", ip: net.ParseIP("100.100.100.100")}
	proxy := httptest.NewServer(newProxy(target, node, log.New(io.Discard, "", 0)))
	defer proxy.Close()

	client := *http.DefaultClient
	client.CheckRedirect = func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }
	request, _ := http.NewRequest(http.MethodGet, proxy.URL, nil)
	request.Host = "workstation.example.ts.net:5173"
	response, err := client.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if location := response.Header.Get("Location"); location != "http://workstation.example.ts.net:5173/login" {
		t.Fatalf("unexpected redirect Location: %q", location)
	}
}

func TestProxyForwardsWebSocketAndRewritesOrigin(t *testing.T) {
	observed := make(chan [2]string, 1)
	backend := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		observed <- [2]string{request.Host, request.Header.Get("Origin")}
		connection, buffer, err := response.(http.Hijacker).Hijack()
		if err != nil {
			t.Errorf("hijack backend connection: %v", err)
			return
		}
		defer connection.Close()
		fmt.Fprint(buffer, "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
		if err := buffer.Flush(); err != nil {
			t.Errorf("flush backend upgrade: %v", err)
		}
	}))
	defer backend.Close()

	target, err := url.Parse(backend.URL)
	if err != nil {
		t.Fatal(err)
	}
	node := tailnetNode{dnsName: "workstation.example.ts.net", ip: net.ParseIP("100.100.100.100")}
	proxy := httptest.NewServer(newProxy(target, node, log.New(io.Discard, "", 0)))
	defer proxy.Close()
	proxyURL, err := url.Parse(proxy.URL)
	if err != nil {
		t.Fatal(err)
	}

	connection, err := net.DialTimeout("tcp", proxyURL.Host, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	fmt.Fprint(connection, "GET /_bun/hmr HTTP/1.1\r\nHost: workstation.example.ts.net:5173\r\nOrigin: http://workstation.example.ts.net:5173\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: test-only\r\n\r\n")

	request := &http.Request{Method: http.MethodGet}
	response, err := http.ReadResponse(bufio.NewReader(connection), request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusSwitchingProtocols {
		t.Fatalf("unexpected status: %d", response.StatusCode)
	}

	select {
	case headers := <-observed:
		if headers[0] != target.Host || headers[1] != target.Scheme+"://"+target.Host {
			t.Fatalf("unexpected backend headers: %#v", headers)
		}
	case <-time.After(time.Second):
		t.Fatal("backend did not receive WebSocket request")
	}
}

func TestProxyRejectsForeignOriginWebSocket(t *testing.T) {
	target, _ := url.Parse("http://127.0.0.1:3000")
	node := tailnetNode{dnsName: "workstation.example.ts.net", ip: net.ParseIP("100.100.100.100")}
	request := httptest.NewRequest(http.MethodGet, "http://workstation.example.ts.net:5173/socket", nil)
	request.Header.Set("Origin", "https://malicious.example")
	request.Header.Set("Connection", "Upgrade")
	request.Header.Set("Upgrade", "websocket")
	response := httptest.NewRecorder()

	newProxy(target, node, log.New(io.Discard, "", 0)).ServeHTTP(response, request)
	if response.Code != http.StatusForbidden {
		t.Fatalf("unexpected status: %d", response.Code)
	}
}
