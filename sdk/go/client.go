package nex

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	"github.com/coder/websocket"
)

// Client is an authenticated device talking to one relay. It is safe for
// concurrent use.
type Client struct {
	// HTTPClient is used for REST calls and WebSocket handshakes; nil means
	// http.DefaultClient. Set it before first use.
	HTTPClient *http.Client

	base *url.URL
	cred DeviceCredential
}

// NewClient returns a client for the relay at server (an http:// or https://
// base URL, optionally with a path prefix) acting as the credential's device.
func NewClient(server string, cred DeviceCredential) (*Client, error) {
	base, err := baseURL(server)
	if err != nil {
		return nil, err
	}
	return &Client{base: base, cred: cred}, nil
}

// Server is the normalized relay base URL (always ends with "/").
func (c *Client) Server() string { return c.base.String() }

// Credential is the device credential this client uses.
func (c *Client) Credential() DeviceCredential { return c.cred }

// DeviceID is this client's device id.
func (c *Client) DeviceID() string { return c.cred.DeviceID }

// Bootstrap creates an account and registers the calling machine as its first
// device. adminKey is required only when the relay is configured with one;
// pass "" otherwise.
func Bootstrap(ctx context.Context, server string, req BootstrapRequest, adminKey string) (DeviceCredential, error) {
	var cred DeviceCredential
	base, err := baseURL(server)
	if err != nil {
		return cred, err
	}
	hdr := http.Header{}
	if adminKey != "" {
		hdr.Set(AdminKeyHeader, adminKey)
	}
	err = doJSON(ctx, nil, http.MethodPost, resolve(base, "v1/bootstrap"), hdr, req, &cred)
	return cred, err
}

// Claim registers a new device into an existing account with a pairing code.
func Claim(ctx context.Context, server string, req ClaimRequest) (DeviceCredential, error) {
	var cred DeviceCredential
	base, err := baseURL(server)
	if err != nil {
		return cred, err
	}
	err = doJSON(ctx, nil, http.MethodPost, resolve(base, "v1/devices/claim"), nil, req, &cred)
	return cred, err
}

// WhoAmI returns the account and device this credential belongs to. It fails
// with an *APIError of status 401 (matching ErrUnauthorized) once revoked.
func (c *Client) WhoAmI(ctx context.Context) (WhoAmI, error) {
	var out WhoAmI
	err := c.do(ctx, http.MethodGet, "v1/me", nil, &out)
	return out, err
}

// Devices lists the devices in this account.
func (c *Client) Devices(ctx context.Context) ([]DeviceInfo, error) {
	var out struct {
		Devices []DeviceInfo `json:"devices"`
	}
	err := c.do(ctx, http.MethodGet, "v1/devices", nil, &out)
	return out.Devices, err
}

// CreatePairingCode creates a one-time code for adding another device.
func (c *Client) CreatePairingCode(ctx context.Context) (PairingCode, error) {
	var out PairingCode
	err := c.do(ctx, http.MethodPost, "v1/pairing-codes", nil, &out)
	return out, err
}

// Rename renames a device in this account.
func (c *Client) Rename(ctx context.Context, deviceID, name string) error {
	body := struct {
		Name string `json:"name"`
	}{name}
	return c.do(ctx, http.MethodPatch, "v1/devices/"+url.PathEscape(deviceID), body, nil)
}

// Revoke revokes a device in this account (possibly this one). It is
// disconnected immediately.
func (c *Client) Revoke(ctx context.Context, deviceID string) error {
	return c.do(ctx, http.MethodDelete, "v1/devices/"+url.PathEscape(deviceID), nil, nil)
}

// Ticket returns a single-use permission to open one stream to service on
// device (a device id, or an exact device name in the account).
func (c *Client) Ticket(ctx context.Context, device, service string) (Ticket, error) {
	var out Ticket
	body := struct {
		Device  string `json:"device"`
		Service string `json:"service"`
	}{device, service}
	err := c.do(ctx, http.MethodPost, "v1/tickets", body, &out)
	return out, err
}

// WSURL resolves a relay path (such as Ticket.Path) to a ws:// or wss:// URL.
func (c *Client) WSURL(path string) (string, error) {
	u := resolve(c.base, path)
	switch u.Scheme {
	case "http":
		u.Scheme = "ws"
	case "https":
		u.Scheme = "wss"
	default:
		return "", fmt.Errorf("nex: unsupported relay scheme %q", u.Scheme)
	}
	return u.String(), nil
}

// Open opens a stream to service on device (id or name).
//
// For a tcp service, send and receive binary frames carrying raw bytes. For a
// ws service, frames are passed to the remote WebSocket server unchanged.
//
// The stream is set up asynchronously: if the host is offline, busy, rejects
// or does not answer, the first Read fails and websocket.CloseStatus(err)
// returns CloseOffline, CloseBusy, CloseRejected or CloseTimeout. ctx only
// bounds the setup; close the returned conn when done.
func (c *Client) Open(ctx context.Context, device, service string) (*websocket.Conn, error) {
	t, err := c.Ticket(ctx, device, service)
	if err != nil {
		return nil, err
	}
	u, err := c.WSURL(t.Path)
	if err != nil {
		return nil, err
	}
	return c.dial(ctx, u, false)
}

// dial opens a WebSocket to the relay, with the device token if authed.
// Handshake HTTP errors are returned as *APIError.
func (c *Client) dial(ctx context.Context, u string, authed bool) (*websocket.Conn, error) {
	opts := &websocket.DialOptions{HTTPClient: c.HTTPClient}
	if authed {
		opts.HTTPHeader = http.Header{"Authorization": {"Bearer " + c.cred.DeviceToken}}
	}
	conn, resp, err := websocket.Dial(ctx, u, opts)
	if err != nil {
		if resp != nil && resp.StatusCode != http.StatusSwitchingProtocols {
			msg := http.StatusText(resp.StatusCode)
			if resp.Body != nil {
				b, _ := io.ReadAll(io.LimitReader(resp.Body, 1024))
				msg = errorMessage(b, msg)
			}
			return nil, &APIError{Status: resp.StatusCode, Message: msg}
		}
		return nil, err
	}
	conn.SetReadLimit(MaxMessageSize)
	return conn, nil
}

func (c *Client) do(ctx context.Context, method, path string, body, out any) error {
	hdr := http.Header{"Authorization": {"Bearer " + c.cred.DeviceToken}}
	return doJSON(ctx, c.HTTPClient, method, resolve(c.base, path), hdr, body, out)
}

// Admin is the relay's admin API, for product backends that map their own
// users onto nex accounts. It holds the admin key: use it on servers only.
type Admin struct {
	// HTTPClient is used for requests; nil means http.DefaultClient.
	HTTPClient *http.Client

	base *url.URL
	key  string
}

// NewAdmin returns an admin API client for the relay at server.
func NewAdmin(server, adminKey string) (*Admin, error) {
	base, err := baseURL(server)
	if err != nil {
		return nil, err
	}
	return &Admin{base: base, key: adminKey}, nil
}

// IssueDevice registers a device for the user req.ExternalAccountID,
// creating the nex account on first use. Hand the credential to that device.
func (a *Admin) IssueDevice(ctx context.Context, req IssueDeviceRequest) (DeviceCredential, error) {
	var cred DeviceCredential
	err := a.do(ctx, http.MethodPost, "v1/admin/devices", req, &cred)
	return cred, err
}

// Devices lists the devices of the user externalAccountID (404 if the
// account does not exist). IsSelf is always false.
func (a *Admin) Devices(ctx context.Context, externalAccountID string) ([]DeviceInfo, error) {
	var out struct {
		Devices []DeviceInfo `json:"devices"`
	}
	err := a.do(ctx, http.MethodGet, accountPath(externalAccountID), nil, &out)
	return out.Devices, err
}

// Revoke revokes one device of the user externalAccountID.
func (a *Admin) Revoke(ctx context.Context, externalAccountID, deviceID string) error {
	path := accountPath(externalAccountID) + "/" + url.PathEscape(deviceID)
	return a.do(ctx, http.MethodDelete, path, nil, nil)
}

func (a *Admin) do(ctx context.Context, method, path string, body, out any) error {
	hdr := http.Header{AdminKeyHeader: {a.key}}
	return doJSON(ctx, a.HTTPClient, method, resolve(a.base, path), hdr, body, out)
}

func accountPath(externalAccountID string) string {
	return "v1/admin/accounts/" + url.PathEscape(externalAccountID) + "/devices"
}

// baseURL normalizes the relay base URL so relative paths keep any prefix.
func baseURL(server string) (*url.URL, error) {
	u, err := url.Parse(server)
	if err != nil {
		return nil, fmt.Errorf("nex: invalid relay URL: %w", err)
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return nil, fmt.Errorf("nex: relay URL must start with http:// or https://, got %q", server)
	}
	if u.Host == "" {
		return nil, fmt.Errorf("nex: relay URL has no host: %q", server)
	}
	u.RawQuery, u.Fragment, u.RawFragment = "", "", ""
	if !strings.HasSuffix(u.Path, "/") {
		u.Path += "/"
		if u.RawPath != "" {
			u.RawPath += "/"
		}
	}
	return u, nil
}

// resolve joins a relative path (escaped, optionally with a query) onto base.
// A leading "/" is ignored so the base's path prefix is kept.
func resolve(base *url.URL, path string) *url.URL {
	rel := strings.TrimLeft(path, "/")
	u, err := url.Parse(base.String() + rel)
	if err != nil {
		// Unparseable input: fall back to treating it as a literal path.
		v := *base
		v.Path += rel
		v.RawPath = ""
		return &v
	}
	return u
}

func doJSON(ctx context.Context, hc *http.Client, method string, u *url.URL, hdr http.Header, body, out any) error {
	if hc == nil {
		hc = http.DefaultClient
	}
	var rd io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		rd = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, u.String(), rd)
	if err != nil {
		return err
	}
	for k, v := range hdr {
		req.Header[k] = v
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	req.Header.Set("Accept", "application/json")
	resp, err := hc.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<10))
		return &APIError{Status: resp.StatusCode, Message: errorMessage(b, http.StatusText(resp.StatusCode))}
	}
	if out == nil {
		_, _ = io.Copy(io.Discard, resp.Body)
		return nil
	}
	if err := json.NewDecoder(resp.Body).Decode(out); err != nil {
		return fmt.Errorf("nex: decoding %s %s response: %w", method, u.Path, err)
	}
	return nil
}

// errorMessage extracts {"error": "..."} from an error body.
func errorMessage(body []byte, fallback string) string {
	var e struct {
		Error string `json:"error"`
	}
	if json.Unmarshal(body, &e) == nil && e.Error != "" {
		return e.Error
	}
	if s := strings.TrimSpace(string(body)); s != "" {
		return s
	}
	return fallback
}
