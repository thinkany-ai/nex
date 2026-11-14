package nex_test

import (
	"net"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"
	nex "github.com/thinkany-ai/nex/sdk/go"
)

// freezableProxy forwards TCP to target. Once frozen, existing connections
// stay open but carry nothing, like a laptop waking on a new network.
type freezableProxy struct {
	ln     net.Listener
	frozen atomic.Bool
	mu     sync.Mutex
	conns  []net.Conn
}

func startProxy(t *testing.T, target string) *freezableProxy {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	noErr(t, err)
	p := &freezableProxy{ln: ln}
	t.Cleanup(func() {
		ln.Close()
		p.mu.Lock()
		for _, c := range p.conns {
			c.Close()
		}
		p.mu.Unlock()
	})
	go func() {
		for {
			in, err := ln.Accept()
			if err != nil {
				return
			}
			out, err := net.Dial("tcp", target)
			if err != nil {
				in.Close()
				continue
			}
			p.mu.Lock()
			p.conns = append(p.conns, in, out)
			p.mu.Unlock()
			frozenAtStart := p.frozen.Load()
			pump := func(dst, src net.Conn) {
				buf := make([]byte, 32<<10)
				for {
					n, err := src.Read(buf)
					if err != nil {
						dst.Close()
						return
					}
					// Connections opened before the freeze go silent;
					// new ones work normally.
					if p.frozen.Load() && !frozenAtStart {
						continue
					}
					if _, err := dst.Write(buf[:n]); err != nil {
						src.Close()
						return
					}
				}
			}
			go pump(out, in)
			go pump(in, out)
		}
	}()
	return p
}

func TestHostReplacesSilentlyDeadConnection(t *testing.T) {
	defer nex.SetKeepaliveForTest(200*time.Millisecond, 300*time.Millisecond)()
	relay := startRelay(t, "")
	proxy := startProxy(t, strings.TrimPrefix(relay, "http://"))
	viaProxy := "http://" + proxy.ln.Addr().String()

	a := bootstrap(t, relay, "acct", "a")
	b := pair(t, a, "b")
	aViaProxy := newClient(t, viaProxy, a.Credential())

	var onlines atomic.Int32
	h := &nex.Host{Client: aViaProxy, Services: []nex.Service{nex.EchoService()}, Logger: quietLogger(),
		OnOnline: func(string) { onlines.Add(1) }}
	ctx := ctxT(t)
	done := make(chan error, 1)
	go func() { done <- h.Run(ctx) }()
	waitOnline(t, b, "a")

	proxy.frozen.Store(true)
	deadline := time.Now().Add(10 * time.Second)
	for onlines.Load() < 2 {
		if time.Now().After(deadline) {
			t.Fatal("host never noticed the dead connection")
		}
		time.Sleep(20 * time.Millisecond)
	}
	got, err := roundtrip(ctx, b, "a", "echo", websocket.MessageText, []byte("still here"))
	noErr(t, err)
	if string(got) != "still here" {
		t.Fatalf("echo returned %q", got)
	}
}
