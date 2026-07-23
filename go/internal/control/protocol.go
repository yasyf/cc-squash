// Package control defines cc-squash's exact business messages over daemonkit's
// persistent session transport.
package control

import (
	"time"

	dkdaemon "github.com/yasyf/daemonkit/daemon"
)

// BusinessBuild is the exact cc-squash control schema identity. Release
// identity is reported independently by OpRuntimeHealth.
const BusinessBuild = "cc-squash.control.v1"

// StatusSchemaVersion is the exact on-disk status snapshot schema.
const StatusSchemaVersion = 1

// Op is a control request operation.
type Op string

// Recognized control request operations.
const (
	OpRuntimeHealth Op = "runtime.health" // exact product runtime identity and readiness
	OpStatus        Op = "status"         // full status snapshot
	OpMint          Op = "mint"           // mint a session token for the proxy
	OpKill          Op = "kill"           // toggle the proxy kill switch
	OpShadow        Op = "shadow"         // toggle proxy shadow mode
	OpGc            Op = "gc"             // sweep the proxy's ref store to its reachable set
)

// EmptyRequest is the exact payload for argument-free operations.
type EmptyRequest struct{}

// ToggleRequest is the exact payload for kill and shadow operations.
type ToggleRequest struct {
	On bool `json:"on"`
}

// RuntimeHealth is the daemon's exact product-visible runtime identity and
// lifecycle state.
type RuntimeHealth struct {
	Build    string         `json:"build"`
	Protocol int            `json:"protocol"`
	PID      int            `json:"pid"`
	State    dkdaemon.State `json:"state"`
	Draining bool           `json:"draining"`
	Busy     bool           `json:"busy"`
}

// StatusSnapshot is the daemon's full status view, returned by OpStatus and
// mirrored on disk for out-of-process readers. Layer-1 minimal; defined here so
// other packages decode it without a forward dependency on the daemon.
type StatusSnapshot struct {
	SchemaVersion int       `json:"schema_version"`
	Version       string    `json:"version"`
	GeneratedAt   time.Time `json:"generated_at"`
	ProxyPort     int       `json:"proxy_port"`
	ProxyMCPort   int       `json:"proxy_mcp_port"`
	ProxyPID      int       `json:"proxy_pid"`
	Sessions      int       `json:"sessions"`
	Kill          bool      `json:"kill"`
	Shadow        bool      `json:"shadow"`
}

// Response is one business-operation reply.
type Response struct {
	OK            bool            `json:"ok"`
	Error         string          `json:"error,omitempty"`
	Port          int             `json:"port,omitempty"`
	MCPPort       int             `json:"mcp_port,omitempty"` // mint: the rmcp retrieve server port
	Token         string          `json:"token,omitempty"`    // mint
	RuntimeHealth *RuntimeHealth  `json:"runtime_health,omitempty"`
	Status        *StatusSnapshot `json:"status,omitempty"` // status
	Kill          bool            `json:"kill,omitempty"`
	Shadow        bool            `json:"shadow,omitempty"`
}
