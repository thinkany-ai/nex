package nex_test

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"

	nex "github.com/thinkany-ai/nex/sdk/go"
)

func TestBootstrapAndPairing(t *testing.T) {
	relay := startRelay(t, "k3y")
	ctx := ctxT(t)

	// The relay has an admin key: bootstrap without (or with a wrong) key is 403.
	_, err := nex.Bootstrap(ctx, relay, nex.BootstrapRequest{AccountName: "team", DeviceName: "laptop"}, "")
	if apiStatus(t, err) != 403 {
		t.Fatalf("bootstrap without key: %v", err)
	}
	_, err = nex.Bootstrap(ctx, relay, nex.BootstrapRequest{AccountName: "team", DeviceName: "laptop"}, "wrong")
	if apiStatus(t, err) != 403 {
		t.Fatalf("bootstrap with wrong key: %v", err)
	}
	cred := must(nex.Bootstrap(ctx, relay, nex.BootstrapRequest{AccountName: "team", DeviceName: "laptop", App: "fastclaw"}, "k3y"))
	if !strings.HasPrefix(cred.DeviceID, "dev_") || cred.DeviceToken == "" || cred.AccountID == "" {
		t.Fatalf("unexpected credential: %+v", cred)
	}
	// A trailing slash on the server URL is fine.
	laptop := newClient(t, relay+"/", cred)
	me := must(laptop.WhoAmI(ctx))
	if me.DeviceName != "laptop" || me.App != "fastclaw" || me.DeviceID != cred.DeviceID || me.AccountID != cred.AccountID {
		t.Fatalf("whoami: %+v", me)
	}

	code := must(laptop.CreatePairingCode(ctx))
	if !regexp.MustCompile(`^[A-Z0-9]{4}-[A-Z0-9]{4}$`).MatchString(code.Code) || code.ExpiresAt == 0 {
		t.Fatalf("pairing code: %+v", code)
	}
	vpsCred := must(nex.Claim(ctx, relay, nex.ClaimRequest{Code: code.Code, DeviceName: "vps", App: "weclaw"}))
	if vpsCred.AccountID != cred.AccountID {
		t.Fatal("claimed device landed in another account")
	}
	// Single use.
	if _, err := nex.Claim(ctx, relay, nex.ClaimRequest{Code: code.Code, DeviceName: "again"}); err == nil {
		t.Fatal("pairing code was reused")
	}
	// Case and dashes do not matter.
	code2 := must(laptop.CreatePairingCode(ctx))
	typed := strings.ToLower(strings.ReplaceAll(code2.Code, "-", ""))
	phoneCred := must(nex.Claim(ctx, relay, nex.ClaimRequest{Code: typed, DeviceName: "phone"}))
	phone := newClient(t, relay, phoneCred)

	devs := must(phone.Devices(ctx))
	if len(devs) != 3 {
		t.Fatalf("want 3 devices, got %d", len(devs))
	}
	var self *nex.DeviceInfo
	for i := range devs {
		if devs[i].IsSelf {
			self = &devs[i]
		}
		if devs[i].Name == "vps" && devs[i].App != "weclaw" {
			t.Fatalf("vps app = %q", devs[i].App)
		}
	}
	if self == nil || self.Name != "phone" || self.Online || self.App != "" {
		t.Fatalf("self: %+v", self)
	}

	// Rename is visible to the account.
	if err := phone.Rename(ctx, vpsCred.DeviceID, "server"); err != nil {
		t.Fatal(err)
	}
	found := false
	for _, d := range must(laptop.Devices(ctx)) {
		if d.ID == vpsCred.DeviceID && d.Name == "server" {
			found = true
		}
	}
	if !found {
		t.Fatal("rename not visible")
	}
}

func TestHostServesAllServiceKinds(t *testing.T) {
	relay := startRelay(t, "")
	laptop := bootstrap(t, relay, "team", "laptop")
	phone := pair(t, laptop, "phone")
	vps := pair(t, laptop, "vps")

	shell := tcpEcho(t, "laptop:")
	raw := tcpEcho(t, "")
	agl := wsEcho(t, "laptop-agl:")
	vpsShell := tcpEcho(t, "vps:")

	upper := nex.HandlerService("upper", nex.KindWS, func(ctx context.Context, c *websocket.Conn, s nex.StreamInfo) {
		for {
			typ, b, err := c.Read(ctx)
			if err != nil {
				return
			}
			reply := strings.ToUpper(string(b)) + " from " + s.FromDeviceID
			if err := c.Write(ctx, typ, []byte(reply)); err != nil {
				return
			}
		}
	})
	// A tcp-kind handler: raw bytes in binary frames, here a byte counter.
	count := nex.HandlerService("count", nex.KindTCP, func(ctx context.Context, c *websocket.Conn, s nex.StreamInfo) {
		_, b, err := c.Read(ctx)
		if err != nil {
			return
		}
		_ = c.Write(ctx, websocket.MessageBinary, []byte(fmt.Sprintf("%d", len(b))))
	})

	host := startHost(t, laptop,
		nex.TCPService("shell", shell.addr),
		nex.TCPService("raw", raw.addr),
		nex.WSService("agl", agl),
		nex.EchoService(),
		upper,
		count,
	)
	startHost(t, vps, nex.TCPService("shell", vpsShell.addr))

	ctx := ctxT(t)
	devs := must(phone.Devices(ctx))
	for _, d := range devs {
		if d.Name == "laptop" && (!d.Online || len(d.Services) != 6) {
			t.Fatalf("laptop listing: %+v", d)
		}
	}

	// Many streams at once, every kind, by name and by id, in several directions.
	type check struct {
		name string
		run  func() (string, error)
		want string
	}
	var checks []check
	for i := 0; i < 5; i++ {
		i := i
		checks = append(checks,
			check{fmt.Sprintf("tcp-%d", i), func() (string, error) {
				p := fmt.Sprintf("hello%d", i)
				return tcpRoundtrip(ctx, phone, "laptop", "shell", p, len("laptop:")+len(p))
			}, fmt.Sprintf("laptop:hello%d", i)},
			check{fmt.Sprintf("ws-%d", i), func() (string, error) {
				b, err := roundtrip(ctx, phone, laptop.DeviceID(), "agl", websocket.MessageText, []byte(fmt.Sprintf("ping%d", i)))
				return string(b), err
			}, fmt.Sprintf("laptop-agl:ping%d", i)},
			check{fmt.Sprintf("echo-text-%d", i), func() (string, error) {
				b, err := roundtrip(ctx, phone, "laptop", "echo", websocket.MessageText, []byte(fmt.Sprintf("e%d", i)))
				return string(b), err
			}, fmt.Sprintf("e%d", i)},
			check{fmt.Sprintf("echo-bin-%d", i), func() (string, error) {
				b, err := roundtrip(ctx, phone, "laptop", "echo", websocket.MessageBinary, []byte{0, 1, byte(i)})
				return fmt.Sprint(b), err
			}, fmt.Sprint([]byte{0, 1, byte(i)})},
			check{fmt.Sprintf("handler-%d", i), func() (string, error) {
				b, err := roundtrip(ctx, phone, "laptop", "upper", websocket.MessageText, []byte(fmt.Sprintf("hi%d", i)))
				return string(b), err
			}, fmt.Sprintf("HI%d from %s", i, phone.DeviceID())},
			check{fmt.Sprintf("tcp-handler-%d", i), func() (string, error) {
				return tcpRoundtrip(ctx, phone, "laptop", "count", strings.Repeat("x", 10+i), 2)
			}, fmt.Sprintf("%d", 10+i)},
			check{fmt.Sprintf("vps->laptop-%d", i), func() (string, error) {
				return tcpRoundtrip(ctx, vps, "laptop", "shell", "v", len("laptop:v"))
			}, "laptop:v"},
			check{fmt.Sprintf("laptop->vps-%d", i), func() (string, error) {
				return tcpRoundtrip(ctx, laptop, "vps", "shell", "l", len("vps:l"))
			}, "vps:l"},
		)
	}
	var wg sync.WaitGroup
	errs := make(chan error, len(checks))
	for _, c := range checks {
		wg.Add(1)
		go func(c check) {
			defer wg.Done()
			got, err := c.run()
			if err != nil {
				errs <- fmt.Errorf("%s: %w", c.name, err)
			} else if got != c.want {
				errs <- fmt.Errorf("%s: got %q, want %q", c.name, got, c.want)
			}
		}(c)
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}

	// A long-lived ws stream carries many messages, and messages far above
	// the WebSocket library's default 32 KiB read limit pass in both
	// directions (local ws server -> host -> relay -> client).
	agls := must(phone.Open(ctx, "laptop", "agl"))
	for i := 0; i < 50; i++ {
		noErr(t, agls.Write(ctx, websocket.MessageText, []byte(fmt.Sprintf("m%d", i))))
		_, b, err := agls.Read(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if string(b) != fmt.Sprintf("laptop-agl:m%d", i) {
			t.Fatalf("got %q", b)
		}
	}
	big := bytes.Repeat([]byte("0123456789abcdef"), 1<<16) // 1 MiB
	noErr(t, agls.Write(ctx, websocket.MessageBinary, big))
	typ, b, err := agls.Read(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if typ != websocket.MessageBinary || !bytes.Equal(b, big) {
		t.Fatalf("big binary message: type %v, %d bytes", typ, len(b))
	}
	_ = agls.Close(websocket.StatusNormalClosure, "")

	big8 := bytes.Repeat([]byte{7}, 8<<20)
	got := must(roundtrip(ctx, phone, "laptop", "echo", websocket.MessageBinary, big8))
	if !bytes.Equal(got, big8) {
		t.Fatalf("8 MiB echo returned %d bytes", len(got))
	}

	// Port forwarding: plain TCP clients on the phone reach the laptop's raw
	// echo, several at once, including a 2 MiB transfer.
	ln := must(net.Listen("tcp", "127.0.0.1:0"))
	fctx, fcancel := context.WithCancel(ctx)
	fdone := make(chan error, 1)
	go func() { fdone <- nex.ForwardTCP(fctx, phone, "laptop", "raw", ln) }()
	var fwg sync.WaitGroup
	for i := 0; i < 4; i++ {
		fwg.Add(1)
		go func(i int) {
			defer fwg.Done()
			conn, err := net.Dial("tcp", ln.Addr().String())
			if err != nil {
				t.Error(err)
				return
			}
			defer conn.Close()
			payload := bytes.Repeat([]byte{byte('a' + i)}, 2<<20)
			go func() { _, _ = conn.Write(payload) }()
			buf := make([]byte, len(payload))
			_ = conn.SetReadDeadline(time.Now().Add(30 * time.Second))
			if _, err := io.ReadFull(conn, buf); err != nil {
				t.Errorf("forward %d: %v", i, err)
				return
			}
			if !bytes.Equal(buf, payload) {
				t.Errorf("forward %d: corrupted data", i)
			}
		}(i)
	}
	fwg.Wait()

	// Closing the local client ends the whole stream: the laptop's local
	// TCP connection gets closed too.
	drain(raw.closed)
	conn := must(net.Dial("tcp", ln.Addr().String()))
	if _, err := conn.Write([]byte("x")); err != nil {
		t.Fatal(err)
	}
	if _, err := io.ReadFull(conn, make([]byte, 1)); err != nil {
		t.Fatal(err)
	}
	conn.Close()
	select {
	case <-raw.closed:
	case <-time.After(5 * time.Second):
		t.Fatal("closing the forwarded client did not close the host's local connection")
	}
	fcancel()
	if err := <-fdone; err != nil {
		t.Fatalf("ForwardTCP: %v", err)
	}
	if _, err := net.Dial("tcp", ln.Addr().String()); err == nil {
		t.Fatal("listener still open after cancel")
	}

	// Closing the local service ends the stream for the client too.
	ws := must(phone.Open(ctx, "laptop", "count"))
	noErr(t, ws.Write(ctx, websocket.MessageBinary, []byte("abc")))
	if _, b, err = ws.Read(ctx); err != nil {
		t.Fatal(err)
	}
	if string(b) != "3" {
		t.Fatalf("count: %q", b)
	}
	if _, _, err := ws.Read(ctx); err == nil {
		t.Fatal("stream stayed open after the handler returned")
	}

	// UpdateServices re-announces on the live connection.
	if err := host.host.UpdateServices([]nex.Service{nex.EchoService(), nex.TCPService("bad name", "x:1")}); !errors.Is(err, nex.ErrBadRequest) {
		t.Fatalf("invalid update accepted: %v", err)
	}
	noErr(t, host.host.UpdateServices([]nex.Service{nex.EchoService(), nex.TCPService("db", raw.addr)}))
	deadline := time.Now().Add(10 * time.Second)
	for {
		var names []string
		for _, d := range must(phone.Devices(ctx)) {
			if d.Name == "laptop" {
				for _, s := range d.Services {
					names = append(names, s.Name+"/"+string(s.Kind))
				}
			}
		}
		if strings.Join(names, ",") == "echo/ws,db/tcp" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("services after update: %v", names)
		}
		time.Sleep(20 * time.Millisecond)
	}
	if _, err := phone.Ticket(ctx, "laptop", "shell"); apiStatus(t, err) != 404 {
		t.Fatalf("removed service still reachable: %v", err)
	}
	if got := must(tcpRoundtrip(ctx, phone, "laptop", "db", "q", 1)); got != "q" {
		t.Fatalf("db: %q", got)
	}
}

func TestRejectWhenLocalServiceDown(t *testing.T) {
	relay := startRelay(t, "")
	a := bootstrap(t, relay, "acct", "a")
	b := pair(t, a, "b")

	dead := freePort(t) // nothing listens here
	startHost(t, a,
		nex.TCPService("db", dead),
		nex.WSService("agl", "ws://"+dead+"/ws"),
	)
	ctx := ctxT(t)
	for _, svc := range []string{"db", "agl"} {
		ws := must(b.Open(ctx, "a", svc))
		_, _, err := ws.Read(ctx)
		if code := websocket.CloseStatus(err); code != nex.CloseRejected {
			t.Fatalf("%s: want close %d, got %d (%v)", svc, nex.CloseRejected, code, err)
		}
		var ce websocket.CloseError
		if !errors.As(err, &ce) || !strings.Contains(ce.Reason, "unavailable") {
			t.Fatalf("%s: reason %q", svc, ce.Reason)
		}
	}
}

func TestErrorsAndIsolation(t *testing.T) {
	relay := startRelay(t, "")
	ctx := ctxT(t)
	laptop := bootstrap(t, relay, "team", "laptop")
	phone := pair(t, laptop, "phone")
	startHost(t, laptop, nex.EchoService())

	// Unknown service.
	_, err := phone.Open(ctx, "laptop", "nope")
	if apiStatus(t, err) != 404 || !strings.Contains(err.Error(), "does not expose") {
		t.Fatalf("unknown service: %v", err)
	}
	// Offline device.
	_, err = laptop.Ticket(ctx, "phone", "echo")
	if apiStatus(t, err) != 404 || !strings.Contains(err.Error(), "offline") {
		t.Fatalf("offline device: %v", err)
	}
	// Unknown device.
	if _, err := phone.Ticket(ctx, "ghost", "echo"); apiStatus(t, err) != 404 {
		t.Fatalf("unknown device: %v", err)
	}

	// A ticket works exactly once; reuse fails the handshake with 401.
	tk := must(phone.Ticket(ctx, "laptop", "echo"))
	u := must(phone.WSURL(tk.Path))
	c1, _, err := websocket.Dial(ctx, u, nil)
	if err != nil {
		t.Fatal(err)
	}
	c1.CloseNow()
	_, resp, err := websocket.Dial(ctx, u, nil)
	if err == nil || resp == nil || resp.StatusCode != 401 {
		t.Fatalf("ticket reuse: %v", err)
	}

	// Another account can neither see nor reach these devices.
	other := bootstrap(t, relay, "other", "x")
	if n := len(must(other.Devices(ctx))); n != 1 {
		t.Fatalf("other account sees %d devices", n)
	}
	if _, err := other.Ticket(ctx, laptop.DeviceID(), "echo"); apiStatus(t, err) != 404 {
		t.Fatalf("cross-account ticket: %v", err)
	}
	if err := other.Revoke(ctx, laptop.DeviceID()); apiStatus(t, err) != 404 {
		t.Fatalf("cross-account revoke: %v", err)
	}
	if err := other.Rename(ctx, laptop.DeviceID(), "pwned"); apiStatus(t, err) != 404 {
		t.Fatalf("cross-account rename: %v", err)
	}

	// Garbage credential: 401, matching ErrUnauthorized.
	bad := newClient(t, relay, nex.DeviceCredential{DeviceToken: "nexd_bogus"})
	if _, err := bad.Devices(ctx); !errors.Is(err, nex.ErrUnauthorized) {
		t.Fatalf("bogus token: %v", err)
	}
	rh := startHostNoWait(t, bad, nex.EchoService())
	if err := rh.wait(t); !errors.Is(err, nex.ErrUnauthorized) {
		t.Fatalf("host with bogus token: %v", err)
	}

	// Invalid services never reach the relay.
	for _, svcs := range [][]nex.Service{
		{nex.TCPService("has space", "127.0.0.1:1")},
		{nex.TCPService(strings.Repeat("a", 65), "127.0.0.1:1")},
		{nex.EchoService(), nex.EchoService()},
		{nex.WSService("w", "http://127.0.0.1:1/")},
		{nex.TCPService("t", "no-port")},
	} {
		h := &nex.Host{Client: laptop, Services: svcs, Logger: quietLogger()}
		if err := h.Run(ctx); !errors.Is(err, nex.ErrBadRequest) {
			t.Fatalf("services %+v: %v", svcs, err)
		}
	}
}

func TestRevokeStopsHost(t *testing.T) {
	relay := startRelay(t, "")
	ctx := ctxT(t)
	laptop := bootstrap(t, relay, "team", "laptop")
	phone := pair(t, laptop, "phone")
	vps := pair(t, laptop, "vps")
	shell := tcpEcho(t, "vps:")
	rh := startHost(t, vps, nex.TCPService("shell", shell.addr))

	open := must(phone.Open(ctx, "vps", "shell"))
	if got := must(tcpRoundtrip(ctx, phone, "vps", "shell", "a", 5)); got != "vps:a" {
		t.Fatal(got)
	}
	noErr(t, laptop.Revoke(ctx, vps.DeviceID()))

	if err := rh.wait(t); !errors.Is(err, nex.ErrRevoked) {
		t.Fatalf("Run after revoke: %v", err)
	}
	rctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	for {
		if _, _, err := open.Read(rctx); err != nil {
			if rctx.Err() != nil {
				t.Fatal("stream to revoked device stayed open")
			}
			break
		}
	}
	if _, err := vps.Devices(ctx); !errors.Is(err, nex.ErrUnauthorized) || apiStatus(t, err) != 401 {
		t.Fatalf("revoked client call: %v", err)
	}
	if _, err := phone.Ticket(ctx, "vps", "shell"); err == nil {
		t.Fatal("revoked device still reachable")
	}
	if n := len(must(phone.Devices(ctx))); n != 2 {
		t.Fatalf("want 2 devices after revoke, got %d", n)
	}
	// Coming back online with the revoked credential fails at the handshake.
	again := startHostNoWait(t, vps, nex.EchoService())
	if err := again.wait(t); !errors.Is(err, nex.ErrUnauthorized) {
		t.Fatalf("restart after revoke: %v", err)
	}
	// Revoking yourself is allowed (logout).
	noErr(t, phone.Revoke(ctx, phone.DeviceID()))
	if _, err := phone.WhoAmI(ctx); !errors.Is(err, nex.ErrUnauthorized) {
		t.Fatalf("after self-revoke: %v", err)
	}
}

func TestSecondHostReplacesFirst(t *testing.T) {
	relay := startRelay(t, "")
	ctx := ctxT(t)
	a := bootstrap(t, relay, "acct", "a")
	b := pair(t, a, "b")

	v1 := tcpEcho(t, "v1:")
	v2 := tcpEcho(t, "v2:")
	first := startHost(t, a, nex.TCPService("db", v1.addr))
	if got := must(tcpRoundtrip(ctx, b, "a", "db", "x", 4)); got != "v1:x" {
		t.Fatal(got)
	}
	second := startHost(t, a, nex.TCPService("db", v2.addr))
	if err := first.wait(t); !errors.Is(err, nex.ErrReplaced) {
		t.Fatalf("first host: %v", err)
	}
	if got := must(tcpRoundtrip(ctx, b, "a", "db", "x", 4)); got != "v2:x" {
		t.Fatalf("after takeover: %q", got)
	}
	select {
	case err := <-second.done:
		t.Fatalf("second host stopped: %v", err)
	default:
	}
}

func TestHostCancelWhileReconnecting(t *testing.T) {
	// Not a fatal condition: cancelling Run while it waits to reconnect
	// returns nil promptly.
	relay := startRelay(t, "")
	a := bootstrap(t, relay, "acct", "a")
	unreachable := newClient(t, "http://"+freePort(t), a.Credential())
	rh := startHostNoWait(t, unreachable, nex.EchoService())
	time.Sleep(50 * time.Millisecond)
	rh.cancel()
	if err := rh.wait(t); err != nil {
		t.Fatalf("Run after cancel: %v", err)
	}
}

func TestAdminAPI(t *testing.T) {
	relay := startRelay(t, "secret")
	ctx := ctxT(t)
	admin := must(nex.NewAdmin(relay, "secret"))
	issue := func(user, device, app string) nex.DeviceCredential {
		return must(admin.IssueDevice(ctx, nex.IssueDeviceRequest{
			ExternalAccountID: user, AccountName: user + " account", DeviceName: device, App: app,
		}))
	}
	// Same external user -> same nex account, across products. The id has
	// characters that must be escaped in the path.
	macCred := issue("user/42 x?", "mac", "fastclaw")
	phoneCred := issue("user/42 x?", "phone", "weclaw")
	strangerCred := issue("user/7", "pc", "fastclaw")
	if macCred.AccountID != phoneCred.AccountID || macCred.AccountID == strangerCred.AccountID {
		t.Fatal("external id mapping is wrong")
	}
	mac := newClient(t, relay, macCred)
	phone := newClient(t, relay, phoneCred)
	stranger := newClient(t, relay, strangerCred)

	startHost(t, mac, nex.EchoService())
	if got := must(roundtrip(ctx, phone, "mac", "echo", websocket.MessageText, []byte("hi"))); string(got) != "hi" {
		t.Fatal(string(got))
	}
	if _, err := stranger.Ticket(ctx, mac.DeviceID(), "echo"); err == nil {
		t.Fatal("stranger reached another account")
	}

	listed := must(admin.Devices(ctx, "user/42 x?"))
	if len(listed) != 2 {
		t.Fatalf("admin list: %+v", listed)
	}
	for _, d := range listed {
		if d.IsSelf {
			t.Fatal("is_self in admin listing")
		}
		if d.ID == mac.DeviceID() && (!d.Online || d.App != "fastclaw" || len(d.Services) != 1) {
			t.Fatalf("mac listing: %+v", d)
		}
	}

	noErr(t, admin.Revoke(ctx, "user/42 x?", phone.DeviceID()))
	if _, err := phone.WhoAmI(ctx); apiStatus(t, err) != 401 {
		t.Fatalf("revoked by admin: %v", err)
	}
	if err := admin.Revoke(ctx, "user/7", mac.DeviceID()); apiStatus(t, err) != 404 {
		t.Fatalf("revoke across accounts: %v", err)
	}
	if _, err := admin.Devices(ctx, "nobody"); apiStatus(t, err) != 404 {
		t.Fatalf("unknown account: %v", err)
	}

	wrong := must(nex.NewAdmin(relay, "nope"))
	if _, err := wrong.Devices(ctx, "user/7"); apiStatus(t, err) != 403 {
		t.Fatalf("wrong key list: %v", err)
	}
	if _, err := wrong.IssueDevice(ctx, nex.IssueDeviceRequest{ExternalAccountID: "u", DeviceName: "d"}); apiStatus(t, err) != 403 {
		t.Fatalf("wrong key issue: %v", err)
	}

	// Without an admin key the admin API is off entirely.
	open := startRelay(t, "")
	if _, err := must(nex.NewAdmin(open, "anything")).Devices(ctx, "u"); apiStatus(t, err) != 403 {
		t.Fatalf("admin API on a keyless relay: %v", err)
	}
}

func drain(c chan struct{}) {
	for {
		select {
		case <-c:
		default:
			return
		}
	}
}
