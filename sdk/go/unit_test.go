package nex

import (
	"errors"
	"fmt"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/coder/websocket"
)

func TestURLs(t *testing.T) {
	cred := DeviceCredential{AccountID: "a", DeviceID: "d", DeviceToken: "t"}
	c, err := NewClient("https://relay.example.com/nex", cred)
	if err != nil {
		t.Fatal(err)
	}
	if got := c.Server(); got != "https://relay.example.com/nex/" {
		t.Fatalf("server: %s", got)
	}
	for path, want := range map[string]string{
		"/v1/connect?ticket=abc": "wss://relay.example.com/nex/v1/connect?ticket=abc",
		"v1/host":                "wss://relay.example.com/nex/v1/host",
	} {
		got, err := c.WSURL(path)
		if err != nil || got != want {
			t.Fatalf("WSURL(%q) = %q, %v; want %q", path, got, err, want)
		}
	}
	if got := resolve(c.base, "v1/devices").String(); got != "https://relay.example.com/nex/v1/devices" {
		t.Fatalf("resolve: %s", got)
	}
	if got := resolve(c.base, accountPath("user/42 x?")).String(); got != "https://relay.example.com/nex/v1/admin/accounts/user%2F42%20x%3F/devices" {
		t.Fatalf("admin path: %s", got)
	}

	c, _ = NewClient("http://127.0.0.1:8080", cred)
	if got, _ := c.WSURL("v1/host"); got != "ws://127.0.0.1:8080/v1/host" {
		t.Fatalf("ws: %s", got)
	}
	c, _ = NewClient("http://127.0.0.1:8080/a/b/", cred)
	if got, _ := c.WSURL("/v1/host"); got != "ws://127.0.0.1:8080/a/b/v1/host" {
		t.Fatalf("prefix with slash: %s", got)
	}

	for _, bad := range []string{"ftp://x", "relay.example.com", "", "http://"} {
		if _, err := NewClient(bad, cred); err == nil {
			t.Errorf("NewClient(%q) accepted", bad)
		}
		if _, err := NewAdmin(bad, "k"); err == nil {
			t.Errorf("NewAdmin(%q) accepted", bad)
		}
	}
}

func TestServiceNames(t *testing.T) {
	for _, ok := range []string{"a", "agl", "ssh-2", "my_svc.v1", strings.Repeat("x", 64)} {
		if !ValidServiceName(ok) {
			t.Errorf("%q rejected", ok)
		}
	}
	for _, bad := range []string{"", "has space", "a/b", "ü", strings.Repeat("x", 65)} {
		if ValidServiceName(bad) {
			t.Errorf("%q accepted", bad)
		}
	}
}

func TestErrors(t *testing.T) {
	var err error = &APIError{Status: 401, Message: "nope"}
	if !errors.Is(err, ErrUnauthorized) {
		t.Fatal("401 should match ErrUnauthorized")
	}
	if errors.Is(&APIError{Status: 404}, ErrUnauthorized) {
		t.Fatal("404 matched ErrUnauthorized")
	}
	if !isFatal(closeErrorFor(CloseReplaced)) || !errors.Is(closeErrorFor(CloseRevoked), ErrRevoked) {
		t.Fatal("close codes not mapped")
	}
	if isFatal(closeErrorFor(1001)) {
		t.Fatal("going-away should be retried")
	}
}

func TestTruncate(t *testing.T) {
	s := strings.Repeat("é", 100) // 200 bytes
	got := truncate(s, 120)
	if len(got) > 120 || !utf8.ValidString(got) {
		t.Fatalf("truncate: %d bytes, valid=%v", len(got), utf8.ValidString(got))
	}
	if truncate("short", 120) != "short" {
		t.Fatal("short string changed")
	}
}

func closeErrorFor(code websocket.StatusCode) error {
	return closeError(fmt.Errorf("read: %w", websocket.CloseError{Code: code, Reason: "r"}))
}
