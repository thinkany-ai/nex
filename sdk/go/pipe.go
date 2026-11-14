package nex

import (
	"context"
	"net"

	"github.com/coder/websocket"
)

// pipeTCP carries a TCP connection over a WebSocket as binary frames, both
// ways. Closing either side ends the whole stream (no half-close). Both
// connections are closed on return. Cancelling ctx tears the stream down.
func pipeTCP(ctx context.Context, ws *websocket.Conn, tcp net.Conn) {
	stop := context.AfterFunc(ctx, func() {
		_ = ws.CloseNow()
		_ = tcp.Close()
	})
	defer stop()

	done := make(chan struct{})
	go func() {
		defer close(done)
		buf := make([]byte, 32<<10)
		for {
			n, err := tcp.Read(buf)
			if n > 0 {
				if werr := ws.Write(context.Background(), websocket.MessageBinary, buf[:n]); werr != nil {
					break
				}
			}
			if err != nil {
				break
			}
		}
		_ = ws.Close(websocket.StatusNormalClosure, "")
	}()

	for {
		_, data, err := ws.Read(context.Background())
		if err != nil {
			break
		}
		// Text frames are not expected on tcp streams; pass their bytes on.
		if _, err := tcp.Write(data); err != nil {
			break
		}
	}
	_ = tcp.Close()
	_ = ws.Close(websocket.StatusNormalClosure, "")
	<-done
}

// pipeWS passes WebSocket messages between a and b unchanged (text stays
// text, binary stays binary) until either side closes, then closes both.
// Ping/pong are handled per connection and never forwarded.
func pipeWS(ctx context.Context, a, b *websocket.Conn) {
	stop := context.AfterFunc(ctx, func() {
		_ = a.CloseNow()
		_ = b.CloseNow()
	})
	defer stop()

	ended := make(chan struct{}, 2)
	pump := func(dst, src *websocket.Conn) {
		defer func() { ended <- struct{}{} }()
		for {
			typ, data, err := src.Read(context.Background())
			if err != nil {
				return
			}
			if err := dst.Write(context.Background(), typ, data); err != nil {
				return
			}
		}
	}
	go pump(a, b)
	go pump(b, a)
	<-ended
	closeBoth(a, b)
	<-ended
}

// echo sends every text or binary message straight back until the peer closes.
func echo(ctx context.Context, ws *websocket.Conn) {
	stop := context.AfterFunc(ctx, func() { _ = ws.CloseNow() })
	defer stop()
	for {
		typ, data, err := ws.Read(context.Background())
		if err != nil {
			break
		}
		if err := ws.Write(context.Background(), typ, data); err != nil {
			break
		}
	}
	_ = ws.Close(websocket.StatusNormalClosure, "")
}

// closeBoth closes two connections concurrently, each with a close handshake.
func closeBoth(a, b *websocket.Conn) {
	done := make(chan struct{})
	go func() {
		_ = a.Close(websocket.StatusNormalClosure, "")
		close(done)
	}()
	_ = b.Close(websocket.StatusNormalClosure, "")
	<-done
}
