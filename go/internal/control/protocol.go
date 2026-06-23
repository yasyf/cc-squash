// Package control defines cc-squash's control-plane wire surface: the
// newline-delimited JSON request/response the CLI sends the daemon over the
// 0600 unix socket, the session token the daemon mints for the proxy, and the
// port-file the daemon publishes its listening port through. It is the single
// source of truth other packages decode against, so it carries the
// StatusSnapshot type itself rather than forward-depending on a daemon package.
package control

import "time"

// ProtocolVersion is bumped on incompatible wire changes. Additive fields stay
// at the same version: an omitempty field a peer omits decodes as the zero
// value, so old and new builds interoperate.
const ProtocolVersion = 1

// Op is a control request operation.
type Op string

// Recognized control request operations.
const (
	OpHealth   Op = "health"   // liveness + version probe
	OpStatus   Op = "status"   // full status snapshot
	OpMint     Op = "mint"     // mint a session token for the proxy
	OpKill     Op = "kill"     // toggle the proxy kill switch
	OpShadow   Op = "shadow"   // toggle proxy shadow mode
	OpShutdown Op = "shutdown" // step down gracefully and release the socket
)

// Request is one client request (one JSON object per line).
type Request struct {
	Proto int  `json:"proto"`
	Op    Op   `json:"op"`
	On    bool `json:"on,omitempty"` // carries the kill/shadow toggle
}

// StatusSnapshot is the daemon's full status view, returned by OpStatus and
// mirrored on disk for out-of-process readers. Layer-1 minimal; defined here so
// other packages decode it without a forward dependency on the daemon.
type StatusSnapshot struct {
	Proto       int       `json:"proto"`
	Version     string    `json:"version"`
	GeneratedAt time.Time `json:"generated_at"`
	ProxyPort   int       `json:"proxy_port"`
	ProxyPID    int       `json:"proxy_pid"`
	Sessions    int       `json:"sessions"`
	Kill        bool      `json:"kill"`
	Shadow      bool      `json:"shadow"`
}

// Response is one server reply (one JSON object per line).
type Response struct {
	Proto   int             `json:"proto"`
	OK      bool            `json:"ok"`
	Error   string          `json:"error,omitempty"`
	Version string          `json:"version,omitempty"` // health
	Port    int             `json:"port,omitempty"`
	Token   string          `json:"token,omitempty"`  // mint
	Status  *StatusSnapshot `json:"status,omitempty"` // status
	Kill    bool            `json:"kill,omitempty"`
	Shadow  bool            `json:"shadow,omitempty"`
}
