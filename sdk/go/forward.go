package nex

import (
	"context"
	"log/slog"
	"net"
	"sync"
)

// ForwardTCP accepts local TCP connections on ln and tunnels each one to the
// tcp service on device (id or name), each over its own stream. It closes ln
// and returns nil when ctx is cancelled (closing open tunnels too), or the
// accept error if ln fails. Failures to open a stream are logged with
// slog.Default() and close that one connection.
func ForwardTCP(ctx context.Context, c *Client, device, service string, ln net.Listener) error {
	stop := context.AfterFunc(ctx, func() { _ = ln.Close() })
	defer stop()

	var wg sync.WaitGroup
	defer wg.Wait()
	for {
		conn, err := ln.Accept()
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return err
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			ws, err := c.Open(ctx, device, service)
			if err != nil {
				slog.Default().Warn("nex: cannot open forwarded stream", "device", device, "service", service, "peer", conn.RemoteAddr().String(), "err", err)
				_ = conn.Close()
				return
			}
			pipeTCP(ctx, ws, conn)
		}()
	}
}
