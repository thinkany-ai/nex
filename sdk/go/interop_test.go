package nex_test

import (
	"context"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"

	nex "github.com/thinkany-ai/nex/sdk/go"
)

// rustCLI joins c's account as a new device using the Rust `nex` CLI and
// returns a function that runs further CLI commands as that device.
func rustCLI(t *testing.T, c *nex.Client, name string) func(ctx context.Context, args ...string) *exec.Cmd {
	t.Helper()
	bin := binPath(t, "NEX_BIN", "nex")
	cfg := filepath.Join(t.TempDir(), "config.json")
	cmdFor := func(ctx context.Context, args ...string) *exec.Cmd {
		cmd := exec.CommandContext(ctx, bin, args...)
		cmd.Env = append(cleanEnv(), "NEX_CONFIG="+cfg, "RUST_LOG=warn")
		return cmd
	}
	code := must(c.CreatePairingCode(ctxT(t)))
	out, err := cmdFor(ctxT(t), "join", code.Code, "--server", c.Server(), "--name", name).CombinedOutput()
	if err != nil {
		t.Fatalf("nex join: %v\n%s", err, out)
	}
	return cmdFor
}

func TestInteropRustHostGoClient(t *testing.T) {
	relay := startRelay(t, "")
	ctx := ctxT(t)
	goDev := bootstrap(t, relay, "team", "go-client")
	cli := rustCLI(t, goDev, "rusty")

	upCtx, stop := context.WithCancel(context.Background())
	up := cli(upCtx, "up", "--echo")
	out := &syncBuffer{}
	up.Stdout, up.Stderr = out, out
	if err := up.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		stop()
		_ = up.Wait()
		if t.Failed() {
			t.Logf("nex up output:\n%s", out.String())
		}
	})
	waitOnline(t, goDev, "rusty")

	for _, m := range []struct {
		typ  websocket.MessageType
		data string
	}{{websocket.MessageText, "hello from go"}, {websocket.MessageBinary, "\x00\x01bin"}} {
		got := must(roundtrip(ctx, goDev, "rusty", "echo", m.typ, []byte(m.data)))
		if string(got) != m.data {
			t.Fatalf("echo via Rust host: %q", got)
		}
	}
	// A long-lived stream.
	ws := must(goDev.Open(ctx, "rusty", "echo"))
	defer ws.CloseNow()
	for i := 0; i < 20; i++ {
		msg := strings.Repeat("x", i*1000+1)
		noErr(t, ws.Write(ctx, websocket.MessageText, []byte(msg)))
		_, b, err := ws.Read(ctx)
		if err != nil || string(b) != msg {
			t.Fatalf("message %d: %v", i, err)
		}
	}
}

func TestInteropGoHostRustPing(t *testing.T) {
	relay := startRelay(t, "")
	goHost := bootstrap(t, relay, "team", "go-host")
	startHost(t, goHost, nex.EchoService())
	cli := rustCLI(t, goHost, "rust-cli")

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	out, err := cli(ctx, "ping", "go-host", "-c", "3", "-i", "0.05").CombinedOutput()
	if err != nil {
		t.Fatalf("nex ping: %v\n%s", err, out)
	}
	if !strings.Contains(string(out), "3 pings") {
		t.Fatalf("unexpected nex ping output:\n%s", out)
	}
}
