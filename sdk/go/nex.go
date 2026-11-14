// Package nex is the Go SDK for the nex device-networking relay.
//
// A device registers once (Bootstrap, Claim, or Admin.IssueDevice) and gets a
// DeviceCredential. With it, a Client lists the account's devices and opens
// streams to services other devices expose, and a Host keeps the device
// online and serves streams to its own local services.
//
// The wire protocol is described in docs/PROTOCOL.md of the nex repository.
package nex

import (
	"errors"
	"fmt"

	"github.com/coder/websocket"
)

// ProtocolVersion is the host control-channel protocol this SDK speaks.
const ProtocolVersion = 1

// AdminKeyHeader carries the relay's admin key.
const AdminKeyHeader = "x-nex-admin-key"

// MaxMessageSize is the largest WebSocket message the relay accepts. Every
// connection the SDK opens uses it as its read limit.
const MaxMessageSize = 16 << 20

// WebSocket close codes used by the relay.
const (
	// CloseBadRequest: the host's hello was invalid (host: fatal).
	CloseBadRequest websocket.StatusCode = 4400
	// CloseRevoked: the device was revoked (host: fatal).
	CloseRevoked websocket.StatusCode = 4401
	// CloseOffline: the target device is offline.
	CloseOffline websocket.StatusCode = 4404
	// CloseReplaced: another host connection with the same credential took
	// over (host: fatal, do not reconnect).
	CloseReplaced websocket.StatusCode = 4409
	// CloseBusy: too many concurrent streams for one of the devices.
	CloseBusy websocket.StatusCode = 4429
	// CloseRejected: the host rejected the stream; the reason says why.
	CloseRejected websocket.StatusCode = 4502
	// CloseTimeout: the host did not accept the stream in time.
	CloseTimeout websocket.StatusCode = 4504
)

// Fatal conditions. Host.Run returns errors wrapping these; test with errors.Is.
var (
	// ErrUnauthorized: the relay rejected the device credential (HTTP 401).
	// An *APIError with status 401 also matches it.
	ErrUnauthorized = errors.New("nex: device credential rejected (revoked or invalid)")
	// ErrRevoked: the device was revoked while online (close code 4401).
	ErrRevoked = errors.New("nex: device revoked")
	// ErrReplaced: another process went online with the same credential
	// (close code 4409). Each process needs its own device.
	ErrReplaced = errors.New("nex: another connection with this device credential took over")
	// ErrBadRequest: the relay refused the hello (close code 4400), or the
	// service list is invalid.
	ErrBadRequest = errors.New("nex: relay refused the service list")
)

// APIError is a non-2xx response from the relay.
type APIError struct {
	Status  int
	Message string
}

func (e *APIError) Error() string {
	return fmt.Sprintf("nex: relay returned %d: %s", e.Status, e.Message)
}

// Is makes a 401 response match ErrUnauthorized.
func (e *APIError) Is(target error) bool {
	return target == ErrUnauthorized && e.Status == 401
}

// ServiceKind says how a stream to a service is carried.
type ServiceKind string

const (
	// KindTCP: raw bytes, carried in binary frames.
	KindTCP ServiceKind = "tcp"
	// KindWS: WebSocket messages passed through unchanged.
	KindWS ServiceKind = "ws"
)

// DeviceCredential is returned once when a device is registered.
// DeviceToken is a bearer secret: store it like a password, never log it.
type DeviceCredential struct {
	AccountID   string `json:"account_id"`
	DeviceID    string `json:"device_id"`
	DeviceToken string `json:"device_token"`
}

// ServiceInfo is a service as the relay sees it.
type ServiceInfo struct {
	Name string      `json:"name"`
	Kind ServiceKind `json:"kind"`
}

// DeviceInfo describes a device in an account.
type DeviceInfo struct {
	ID   string `json:"id"`
	Name string `json:"name"`
	// App is the product that registered the device; empty if unknown.
	App string `json:"app"`
	// Online is true while the device holds a host control connection.
	Online bool `json:"online"`
	// Services the device exposes (empty while offline).
	Services  []ServiceInfo `json:"services"`
	CreatedAt int64         `json:"created_at"`
	// LastSeenAt is Unix seconds; 0 if never seen.
	LastSeenAt int64 `json:"last_seen_at"`
	// IsSelf is true for the device making the request.
	IsSelf bool `json:"is_self"`
}

// Ticket is a single-use, short-lived (60s) permission to open one stream.
type Ticket struct {
	Ticket    string      `json:"ticket"`
	ExpiresAt int64       `json:"expires_at"`
	DeviceID  string      `json:"device_id"`
	Service   ServiceInfo `json:"service"`
	// Path is relative to the relay root, e.g. "/v1/connect?ticket=...".
	Path string `json:"path"`
}

// PairingCode adds another device to the account (single use, 10 minutes).
type PairingCode struct {
	// Code is formatted XXXX-XXXX; case and dashes are ignored when claiming.
	Code      string `json:"code"`
	ExpiresAt int64  `json:"expires_at"`
}

// WhoAmI is the account and device a credential belongs to.
type WhoAmI struct {
	AccountID  string `json:"account_id"`
	DeviceID   string `json:"device_id"`
	DeviceName string `json:"device_name"`
	App        string `json:"app"`
}

// BootstrapRequest creates an account with its first device.
type BootstrapRequest struct {
	AccountName string `json:"account_name"`
	DeviceName  string `json:"device_name"`
	App         string `json:"app,omitempty"`
}

// ClaimRequest registers a device into an existing account with a pairing code.
type ClaimRequest struct {
	Code       string `json:"code"`
	DeviceName string `json:"device_name"`
	App        string `json:"app,omitempty"`
}

// IssueDeviceRequest registers a device for a product user (admin API). The
// nex account is created on first use of ExternalAccountID.
type IssueDeviceRequest struct {
	ExternalAccountID string `json:"external_account_id"`
	// AccountName is used when the account is created; defaults to the id.
	AccountName string `json:"account_name,omitempty"`
	DeviceName  string `json:"device_name"`
	App         string `json:"app,omitempty"`
}

// ValidServiceName reports whether name is 1-64 chars of A-Z a-z 0-9 . _ -.
// App names follow the same rule.
func ValidServiceName(name string) bool {
	if len(name) == 0 || len(name) > 64 {
		return false
	}
	for i := 0; i < len(name); i++ {
		c := name[i]
		ok := c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
			c == '.' || c == '_' || c == '-'
		if !ok {
			return false
		}
	}
	return true
}
