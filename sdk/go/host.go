package nex

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/url"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/coder/websocket"
)

// StreamInfo describes an incoming stream.
type StreamInfo struct {
	StreamID     string
	Service      string
	FromDeviceID string
}

// HandlerFunc serves one stream in-process. conn is the accepted stream; it
// is closed after the handler returns. ctx is cancelled when Host.Run stops.
type HandlerFunc func(ctx context.Context, conn *websocket.Conn, s StreamInfo)

type serviceTarget int

const (
	targetTCP serviceTarget = iota
	targetWS
	targetEcho
	targetHandler
)

// Service is a service the host exposes. Build it with TCPService, WSService,
// EchoService or HandlerService.
type Service struct {
	Name string
	Kind ServiceKind

	target  serviceTarget
	addr    string // tcp host:port, or ws URL
	handler HandlerFunc
}

// TCPService exposes a local TCP server at addr ("host:port").
func TCPService(name, addr string) Service {
	return Service{Name: name, Kind: KindTCP, target: targetTCP, addr: addr}
}

// WSService exposes a local WebSocket server at url ("ws://..." or "wss://...").
func WSService(name, url string) Service {
	return Service{Name: name, Kind: KindWS, target: targetWS, addr: url}
}

// EchoService is the conventional built-in "echo" ws service: every text or
// binary message is sent straight back. `nex ping` relies on it.
func EchoService() Service {
	return Service{Name: "echo", Kind: KindWS, target: targetEcho}
}

// HandlerService serves streams in-process, without a local port. For kind
// KindTCP the peer sends and expects binary frames of raw bytes; for KindWS
// any messages.
func HandlerService(name string, kind ServiceKind, fn HandlerFunc) Service {
	return Service{Name: name, Kind: kind, target: targetHandler, handler: fn}
}

// Info is the service as announced to the relay.
func (s Service) Info() ServiceInfo { return ServiceInfo{Name: s.Name, Kind: s.Kind} }

func (s Service) validate() error {
	if !ValidServiceName(s.Name) {
		return fmt.Errorf("invalid service name %q: use 1-64 chars of A-Z a-z 0-9 . _ -", s.Name)
	}
	if s.Kind != KindTCP && s.Kind != KindWS {
		return fmt.Errorf("service %q: kind must be %q or %q", s.Name, KindTCP, KindWS)
	}
	switch s.target {
	case targetTCP:
		if _, _, err := net.SplitHostPort(s.addr); err != nil {
			return fmt.Errorf("service %q: invalid address %q: %v", s.Name, s.addr, err)
		}
	case targetWS:
		u, err := url.Parse(s.addr)
		if err != nil || (u.Scheme != "ws" && u.Scheme != "wss") || u.Host == "" {
			return fmt.Errorf("service %q: invalid WebSocket URL %q (want ws:// or wss://)", s.Name, s.addr)
		}
	case targetHandler:
		if s.handler == nil {
			return fmt.Errorf("service %q: nil handler", s.Name)
		}
	}
	return nil
}

func validateServices(svcs []Service) error {
	if len(svcs) > 64 {
		return fmt.Errorf("%w: too many services (%d > 64)", ErrBadRequest, len(svcs))
	}
	seen := make(map[string]bool, len(svcs))
	for _, s := range svcs {
		if err := s.validate(); err != nil {
			return fmt.Errorf("%w: %v", ErrBadRequest, err)
		}
		if seen[s.Name] {
			return fmt.Errorf("%w: duplicate service %q", ErrBadRequest, s.Name)
		}
		seen[s.Name] = true
	}
	return nil
}

// Host keeps a device online on the relay and serves streams to its services.
//
// Fill in Client and Services, then call Run. Only one Host may be online per
// credential at a time; a second one takes over and the first stops with
// ErrReplaced.
type Host struct {
	Client   *Client
	Services []Service
	// Logger receives connection and stream events; nil means slog.Default().
	Logger *slog.Logger
	// OnOnline is called (on the control loop; return quickly) each time
	// the device comes online, including after reconnects.
	OnOnline func(deviceID string)

	mu      sync.Mutex
	svcs    []Service
	svcsSet bool
	live    *websocket.Conn // current control connection, nil while offline
	helloMu sync.Mutex      // orders hello frames
}

// Run stays online until ctx is cancelled, reconnecting with backoff (1s
// doubling to 30s, reset after a session that lasted over 30s). It returns
// nil when ctx is cancelled, and otherwise only for unrecoverable conditions:
// errors wrapping ErrUnauthorized, ErrRevoked, ErrReplaced or ErrBadRequest.
// Streams being served are closed before Run returns.
func (h *Host) Run(ctx context.Context) error {
	if h.Client == nil {
		return errors.New("nex: Host.Client is nil")
	}
	h.mu.Lock()
	if !h.svcsSet {
		h.svcs = append([]Service(nil), h.Services...)
		h.svcsSet = true
	}
	svcs := h.svcs
	h.mu.Unlock()
	if err := validateServices(svcs); err != nil {
		return err
	}

	runCtx, cancel := context.WithCancel(ctx)
	var streams sync.WaitGroup
	defer streams.Wait()
	defer cancel()

	log := h.logger()
	backoff := time.Second
	for {
		started := time.Now()
		err := h.session(runCtx, &streams)
		if ctx.Err() != nil {
			return nil
		}
		if isFatal(err) {
			log.Error("nex host stopped", "err", err)
			return err
		}
		if time.Since(started) > 30*time.Second {
			backoff = time.Second
		}
		log.Warn("nex relay connection lost; reconnecting", "err", err, "in", backoff)
		t := time.NewTimer(backoff)
		select {
		case <-ctx.Done():
			t.Stop()
			return nil
		case <-t.C:
		}
		backoff = min(backoff*2, 30*time.Second)
	}
}

// UpdateServices replaces the exposed services. If the host is online, the
// new list is announced right away on the live connection; it is also used
// by later reconnects (and overrides Host.Services if called before Run).
func (h *Host) UpdateServices(svcs []Service) error {
	if err := validateServices(svcs); err != nil {
		return err
	}
	h.mu.Lock()
	h.svcs = append([]Service(nil), svcs...)
	h.svcsSet = true
	conn := h.live
	h.mu.Unlock()
	if conn == nil {
		return nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	return h.sendHello(ctx, conn)
}

func (h *Host) logger() *slog.Logger {
	if h.Logger != nil {
		return h.Logger
	}
	return slog.Default()
}

func isFatal(err error) bool {
	return errors.Is(err, ErrUnauthorized) || errors.Is(err, ErrRevoked) ||
		errors.Is(err, ErrReplaced) || errors.Is(err, ErrBadRequest)
}

type hostMsg struct {
	Type     string        `json:"type"`
	Protocol int           `json:"protocol,omitempty"`
	Services []ServiceInfo `json:"services,omitempty"`
	StreamID string        `json:"stream_id,omitempty"`
	Reason   string        `json:"reason,omitempty"`
}

type serverMsg struct {
	Type         string `json:"type"`
	DeviceID     string `json:"device_id"`
	StreamID     string `json:"stream_id"`
	Service      string `json:"service"`
	FromDeviceID string `json:"from_device_id"`
	Message      string `json:"message"`
}

func (h *Host) sendHello(ctx context.Context, conn *websocket.Conn) error {
	h.helloMu.Lock()
	defer h.helloMu.Unlock()
	h.mu.Lock()
	infos := make([]ServiceInfo, 0, len(h.svcs))
	for _, s := range h.svcs {
		infos = append(infos, s.Info())
	}
	h.mu.Unlock()
	// Services must be present even when empty, so marshal explicitly.
	b, err := json.Marshal(struct {
		Type     string        `json:"type"`
		Protocol int           `json:"protocol"`
		Services []ServiceInfo `json:"services"`
	}{"hello", ProtocolVersion, infos})
	if err != nil {
		return err
	}
	return conn.Write(ctx, websocket.MessageText, b)
}

func (h *Host) lookup(name string) (Service, bool) {
	h.mu.Lock()
	defer h.mu.Unlock()
	for _, s := range h.svcs {
		if s.Name == name {
			return s, true
		}
	}
	return Service{}, false
}

// session runs one control connection until it ends. It returns ctx.Err()
// when cancelled, a fatal error, or an error worth retrying.
// How often the host probes the relay, and how long it waits for the pong.
// Together they bound how long a silently dead connection goes unnoticed.
var (
	keepaliveInterval = 30 * time.Second
	keepaliveTimeout  = 35 * time.Second
)

func (h *Host) session(ctx context.Context, streams *sync.WaitGroup) error {
	log := h.logger()
	u, err := h.Client.WSURL("v1/host")
	if err != nil {
		return err
	}
	dctx, dcancel := context.WithTimeout(ctx, 30*time.Second)
	conn, err := h.Client.dial(dctx, u, true)
	dcancel()
	if err != nil {
		var apiErr *APIError
		if errors.As(err, &apiErr) && apiErr.Status == 401 {
			return fmt.Errorf("%w: %s", ErrUnauthorized, apiErr.Message)
		}
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return err
	}
	defer conn.CloseNow()

	wctx, wcancel := context.WithTimeout(ctx, 10*time.Second)
	err = h.sendHello(wctx, conn)
	wcancel()
	if err != nil {
		return err
	}
	h.mu.Lock()
	h.live = conn
	h.mu.Unlock()
	defer func() {
		h.mu.Lock()
		if h.live == conn {
			h.live = nil
		}
		h.mu.Unlock()
	}()

	// On cancel, close politely; the read below then returns.
	stop := context.AfterFunc(ctx, func() {
		_ = conn.Close(websocket.StatusNormalClosure, "host stopping")
	})
	defer stop()

	// The relay pings every 20s, but coder/websocket answers those inside
	// the library, so we cannot watch them. Probe the other way instead: a
	// connection that died silently (sleep, network switch) fails the ping
	// and is replaced.
	kctx, kcancel := context.WithCancel(ctx)
	defer kcancel()
	go func() {
		t := time.NewTicker(keepaliveInterval)
		defer t.Stop()
		for {
			select {
			case <-kctx.Done():
				return
			case <-t.C:
				pctx, pcancel := context.WithTimeout(kctx, keepaliveTimeout)
				err := conn.Ping(pctx)
				pcancel()
				if err != nil && kctx.Err() == nil {
					log.Warn("nex: relay stopped answering; reconnecting", "err", err)
					_ = conn.CloseNow()
					return
				}
			}
		}
	}()

	for {
		typ, data, err := conn.Read(context.Background())
		if err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			return closeError(err)
		}
		if typ != websocket.MessageText {
			continue
		}
		var msg serverMsg
		if err := json.Unmarshal(data, &msg); err != nil {
			log.Debug("nex: ignoring malformed relay frame", "err", err)
			continue
		}
		switch msg.Type {
		case "ready":
			log.Info("nex host online", "device_id", msg.DeviceID)
			if h.OnOnline != nil {
				h.OnOnline(msg.DeviceID)
			}
		case "open":
			log.Debug("nex incoming stream", "stream_id", msg.StreamID, "service", msg.Service, "from", msg.FromDeviceID)
			streams.Add(1)
			go func(m serverMsg) {
				defer streams.Done()
				h.serveStream(ctx, conn, StreamInfo{StreamID: m.StreamID, Service: m.Service, FromDeviceID: m.FromDeviceID})
			}(msg)
		case "error":
			log.Warn("nex relay error", "message", msg.Message)
		default:
			// Unknown types are ignored for forward compatibility.
		}
	}
}

// closeError maps a control-connection read error to fatal sentinels.
func closeError(err error) error {
	var ce websocket.CloseError
	reason := ""
	if errors.As(err, &ce) {
		reason = ce.Reason
	}
	switch websocket.CloseStatus(err) {
	case CloseRevoked:
		return fmt.Errorf("%w: %s", ErrRevoked, reason)
	case CloseBadRequest:
		return fmt.Errorf("%w: %s", ErrBadRequest, reason)
	case CloseReplaced:
		return fmt.Errorf("%w: %s", ErrReplaced, reason)
	}
	return fmt.Errorf("nex: relay connection ended: %w", err)
}

func (h *Host) serveStream(ctx context.Context, ctrl *websocket.Conn, s StreamInfo) {
	log := h.logger().With("stream_id", s.StreamID, "service", s.Service)
	reject := func(reason string) {
		log.Warn("nex: rejecting stream", "reason", reason)
		b, _ := json.Marshal(hostMsg{Type: "reject", StreamID: s.StreamID, Reason: truncate(reason, 120)})
		wctx, cancel := context.WithTimeout(ctx, 10*time.Second)
		defer cancel()
		if err := ctrl.Write(wctx, websocket.MessageText, b); err != nil {
			log.Debug("nex: sending reject failed", "err", err)
		}
	}
	svc, ok := h.lookup(s.Service)
	if !ok {
		reject(fmt.Sprintf("unknown service '%s'", s.Service))
		return
	}

	// Connect to the local service first so a dead service is reported to
	// the caller as a rejection instead of an empty stream.
	var local net.Conn
	var localWS *websocket.Conn
	switch svc.target {
	case targetTCP:
		d := net.Dialer{Timeout: 10 * time.Second}
		c, err := d.DialContext(ctx, "tcp", svc.addr)
		if err != nil {
			reject("local service unavailable: " + err.Error())
			return
		}
		local = c
	case targetWS:
		dctx, cancel := context.WithTimeout(ctx, 10*time.Second)
		c, _, err := websocket.Dial(dctx, svc.addr, nil)
		cancel()
		if err != nil {
			reject("local service unavailable: " + err.Error())
			return
		}
		c.SetReadLimit(MaxMessageSize)
		localWS = c
	}

	ws, err := h.accept(ctx, s.StreamID)
	if err != nil {
		log.Warn("nex: accepting stream failed", "err", err)
		if local != nil {
			_ = local.Close()
		}
		if localWS != nil {
			_ = localWS.CloseNow()
		}
		return
	}
	switch svc.target {
	case targetTCP:
		pipeTCP(ctx, ws, local)
	case targetWS:
		pipeWS(ctx, ws, localWS)
	case targetEcho:
		echo(ctx, ws)
	case targetHandler:
		func() {
			defer func() {
				if r := recover(); r != nil {
					log.Error("nex: stream handler panicked", "panic", r)
				}
			}()
			hctx, cancel := context.WithCancel(ctx)
			defer cancel()
			svc.handler(hctx, ws, s)
		}()
		_ = ws.Close(websocket.StatusNormalClosure, "")
	}
	log.Debug("nex stream closed")
}

// accept opens the host side of a stream (must happen within 15s of open).
func (h *Host) accept(ctx context.Context, streamID string) (*websocket.Conn, error) {
	u, err := h.Client.WSURL("v1/accept?stream_id=" + url.QueryEscape(streamID))
	if err != nil {
		return nil, err
	}
	dctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	return h.Client.dial(dctx, u, true)
}

// truncate shortens s to at most n bytes on a UTF-8 boundary (close-frame
// reasons are limited to 123 bytes).
func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	for n > 0 && !utf8.RuneStart(s[n]) {
		n--
	}
	return s[:n]
}
