package nex_test

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"

	nex "github.com/thinkany-ai/nex/sdk/go"
)

// binPath finds a built nex binary via env or the repo's target/release.
func binPath(t *testing.T, env, name string) string {
	t.Helper()
	p := os.Getenv(env)
	if p == "" {
		p = filepath.Join("..", "..", "target", "release", name)
	}
	abs, err := filepath.Abs(p)
	if err != nil {
		t.Skipf("%s: %v", name, err)
	}
	if _, err := os.Stat(abs); err != nil {
		t.Skipf("%s not found at %s (set %s or run `cargo build --release` in the repo root)", name, abs, env)
	}
	return abs
}

func freePort(t *testing.T) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	_ = ln.Close()
	return addr
}

// syncBuffer collects process output safely.
type syncBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.Write(p)
}

func (s *syncBuffer) String() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.String()
}

// startRelay runs a fresh nex-server on a free port and returns its URL.
func startRelay(t *testing.T, adminKey string) string {
	t.Helper()
	bin := binPath(t, "NEX_SERVER_BIN", "nex-server")
	for attempt := 0; attempt < 5; attempt++ {
		addr := freePort(t)
		args := []string{"--listen", addr, "--db", ":memory:"}
		if adminKey != "" {
			args = append(args, "--admin-key", adminKey)
		}
		cmd := exec.Command(bin, args...)
		cmd.Env = append(cleanEnv(), "RUST_LOG=warn")
		out := &syncBuffer{}
		cmd.Stdout, cmd.Stderr = out, out
		if err := cmd.Start(); err != nil {
			t.Fatalf("start relay: %v", err)
		}
		exited := make(chan struct{})
		go func() { _ = cmd.Wait(); close(exited) }()
		url := "http://" + addr
		if waitHealthy(url, exited) {
			t.Cleanup(func() {
				_ = cmd.Process.Kill()
				<-exited
				if t.Failed() {
					t.Logf("relay output:\n%s", out.String())
				}
			})
			return url
		}
		_ = cmd.Process.Kill()
		<-exited
		t.Logf("relay on %s did not start (retrying): %s", addr, out.String())
	}
	t.Fatal("could not start relay")
	return ""
}

// cleanEnv is the environment without NEX_* variables.
func cleanEnv() []string {
	var env []string
	for _, kv := range os.Environ() {
		if !strings.HasPrefix(kv, "NEX_") {
			env = append(env, kv)
		}
	}
	return env
}

func waitHealthy(url string, exited <-chan struct{}) bool {
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		select {
		case <-exited:
			return false
		default:
		}
		resp, err := http.Get(url + "/healthz")
		if err == nil {
			b, _ := io.ReadAll(resp.Body)
			resp.Body.Close()
			if resp.StatusCode == 200 && strings.TrimSpace(string(b)) == "ok" {
				return true
			}
		}
		time.Sleep(20 * time.Millisecond)
	}
	return false
}

func ctxT(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func quietLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

// must panics on error; the testing package still runs cleanups. Use it
// only on the test goroutine.
func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

func noErr(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func newClient(t *testing.T, relay string, cred nex.DeviceCredential) *nex.Client {
	t.Helper()
	return must(nex.NewClient(relay, cred))
}

// bootstrap creates a new account with one device.
func bootstrap(t *testing.T, relay, account, device string) *nex.Client {
	t.Helper()
	cred := must(nex.Bootstrap(ctxT(t), relay, nex.BootstrapRequest{AccountName: account, DeviceName: device}, ""))
	return newClient(t, relay, cred)
}

// pair adds a device to c's account.
func pair(t *testing.T, c *nex.Client, device string) *nex.Client {
	t.Helper()
	code := must(c.CreatePairingCode(ctxT(t)))
	cred := must(nex.Claim(ctxT(t), c.Server(), nex.ClaimRequest{Code: code.Code, DeviceName: device}))
	return newClient(t, c.Server(), cred)
}

type runningHost struct {
	host   *nex.Host
	cancel context.CancelFunc
	done   chan error
	online chan string
}

// startHost runs a Host in the background and waits until it is online.
func startHost(t *testing.T, c *nex.Client, svcs ...nex.Service) *runningHost {
	t.Helper()
	rh := startHostNoWait(t, c, svcs...)
	select {
	case <-rh.online:
	case err := <-rh.done:
		t.Fatalf("host stopped before going online: %v", err)
	case <-time.After(10 * time.Second):
		t.Fatal("host never came online")
	}
	return rh
}

func startHostNoWait(t *testing.T, c *nex.Client, svcs ...nex.Service) *runningHost {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	rh := &runningHost{cancel: cancel, done: make(chan error, 1), online: make(chan string, 16)}
	rh.host = &nex.Host{
		Client:   c,
		Services: svcs,
		Logger:   quietLogger(),
		OnOnline: func(id string) {
			select {
			case rh.online <- id:
			default:
			}
		},
	}
	go func() { rh.done <- rh.host.Run(ctx) }()
	t.Cleanup(func() {
		cancel()
		select {
		case <-rh.done:
		case <-time.After(10 * time.Second):
			t.Error("host did not stop after cancel")
		}
	})
	return rh
}

// wait returns Run's result, failing the test after a timeout.
func (rh *runningHost) wait(t *testing.T) error {
	t.Helper()
	select {
	case err := <-rh.done:
		rh.done <- err // let cleanup see it too
		return err
	case <-time.After(10 * time.Second):
		t.Fatal("host did not stop")
		return nil
	}
}

// waitOnline polls until device is online (for hosts we do not control).
func waitOnline(t *testing.T, observer *nex.Client, device string) {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	for time.Now().Before(deadline) {
		devs := must(observer.Devices(ctxT(t)))
		for _, d := range devs {
			if (d.ID == device || d.Name == device) && d.Online {
				return
			}
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("device %s never came online", device)
}

// tcpEcho is a TCP server that prefixes replies; closed conns are reported.
type tcpEchoServer struct {
	addr   string
	closed chan struct{}
}

func tcpEcho(t *testing.T, prefix string) *tcpEchoServer {
	t.Helper()
	ln := must(net.Listen("tcp", "127.0.0.1:0"))
	s := &tcpEchoServer{addr: ln.Addr().String(), closed: make(chan struct{}, 1024)}
	var wg sync.WaitGroup
	t.Cleanup(func() { _ = ln.Close(); wg.Wait() })
	wg.Add(1)
	go func() {
		defer wg.Done()
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			wg.Add(1)
			go func() {
				defer wg.Done()
				defer func() { s.closed <- struct{}{} }()
				defer conn.Close()
				buf := make([]byte, 64<<10)
				for {
					n, err := conn.Read(buf)
					if n > 0 {
						out := append([]byte(prefix), buf[:n]...)
						if _, werr := conn.Write(out); werr != nil {
							return
						}
					}
					if err != nil {
						return
					}
				}
			}()
		}
	}()
	return s
}

// wsEcho is a local WebSocket server that echoes, prefixing text messages.
func wsEcho(t *testing.T, prefix string) string {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		c.SetReadLimit(nex.MaxMessageSize)
		defer c.CloseNow()
		for {
			typ, data, err := c.Read(r.Context())
			if err != nil {
				return
			}
			if typ == websocket.MessageText {
				data = append([]byte(prefix), data...)
			}
			if err := c.Write(r.Context(), typ, data); err != nil {
				return
			}
		}
	}))
	t.Cleanup(srv.Close)
	return "ws" + strings.TrimPrefix(srv.URL, "http") + "/ws"
}

func roundtrip(ctx context.Context, c *nex.Client, device, service string, typ websocket.MessageType, payload []byte) ([]byte, error) {
	ws, err := c.Open(ctx, device, service)
	if err != nil {
		return nil, err
	}
	defer ws.CloseNow()
	if err := ws.Write(ctx, typ, payload); err != nil {
		return nil, err
	}
	got, rt, err := ws.Read(ctx)
	if err != nil {
		return nil, err
	}
	if got != typ {
		return nil, fmt.Errorf("got message type %v, want %v", got, typ)
	}
	_ = ws.Close(websocket.StatusNormalClosure, "")
	return rt, nil
}

// tcpRoundtrip sends payload as binary and reads until want bytes arrive.
func tcpRoundtrip(ctx context.Context, c *nex.Client, device, service, payload string, wantLen int) (string, error) {
	ws, err := c.Open(ctx, device, service)
	if err != nil {
		return "", err
	}
	defer ws.CloseNow()
	if err := ws.Write(ctx, websocket.MessageBinary, []byte(payload)); err != nil {
		return "", err
	}
	var got []byte
	for len(got) < wantLen {
		typ, b, err := ws.Read(ctx)
		if err != nil {
			return "", err
		}
		if typ != websocket.MessageBinary {
			return "", errors.New("tcp stream sent a text frame")
		}
		got = append(got, b...)
	}
	_ = ws.Close(websocket.StatusNormalClosure, "")
	return string(got), nil
}

func apiStatus(t *testing.T, err error) int {
	t.Helper()
	var apiErr *nex.APIError
	if !errors.As(err, &apiErr) {
		t.Fatalf("expected *nex.APIError, got %T: %v", err, err)
	}
	return apiErr.Status
}
