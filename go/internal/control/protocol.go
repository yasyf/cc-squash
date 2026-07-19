// Package control defines cc-squash's exact business messages over daemonkit's
// persistent session transport.
package control

import "time"

// BusinessBuild is the exact cc-squash control schema identity. Release
// identity is carried independently as daemonkit's LifecycleBuild.
const BusinessBuild = "cc-squash.control.v2"

// Op is a control request operation.
type Op string

// Recognized control request operations.
const (
	OpStatus Op = "status" // full status snapshot
	OpMint   Op = "mint"   // mint a session token for the proxy
	OpKill   Op = "kill"   // toggle the proxy kill switch
	OpShadow Op = "shadow" // toggle proxy shadow mode
	OpGc     Op = "gc"     // sweep the proxy's ref store to its reachable set
)

// EmptyRequest is the exact payload for argument-free operations.
type EmptyRequest struct{}

// ToggleRequest is the exact payload for kill and shadow operations.
type ToggleRequest struct {
	On bool `json:"on"`
}

// StatusSnapshot is the daemon's full status view, returned by OpStatus and
// mirrored on disk for out-of-process readers. Layer-1 minimal; defined here so
// other packages decode it without a forward dependency on the daemon.
type StatusSnapshot struct {
	Version     string    `json:"version"`
	GeneratedAt time.Time `json:"generated_at"`
	ProxyPort   int       `json:"proxy_port"`
	ProxyMCPort int       `json:"proxy_mcp_port"`
	ProxyPID    int       `json:"proxy_pid"`
	Sessions    int       `json:"sessions"`
	Kill        bool      `json:"kill"`
	Shadow      bool      `json:"shadow"`
}

// Response is one business-operation reply.
type Response struct {
	OK      bool            `json:"ok"`
	Error   string          `json:"error,omitempty"`
	Port    int             `json:"port,omitempty"`
	MCPPort int             `json:"mcp_port,omitempty"` // mint: the rmcp retrieve server port
	Token   string          `json:"token,omitempty"`    // mint
	Status  *StatusSnapshot `json:"status,omitempty"`   // status
	Kill    bool            `json:"kill,omitempty"`
	Shadow  bool            `json:"shadow,omitempty"`
}
