package nex

import "time"

// SetKeepaliveForTest shortens the host keepalive and returns a restore func.
func SetKeepaliveForTest(interval, timeout time.Duration) func() {
	oldI, oldT := keepaliveInterval, keepaliveTimeout
	keepaliveInterval, keepaliveTimeout = interval, timeout
	return func() { keepaliveInterval, keepaliveTimeout = oldI, oldT }
}
