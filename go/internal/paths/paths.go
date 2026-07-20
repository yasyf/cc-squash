// Package paths names cc-squash's product-specific state beside daemonkit's
// canonical daemon state, socket, log, and lock paths.
package paths

import (
	"path/filepath"

	dkpaths "github.com/yasyf/daemonkit/paths"
)

var daemon = dkpaths.Paths{App: ".cc-squash"}

func path(leaf string) string {
	return filepath.Join(daemon.StateDir(), leaf)
}

// StateDir is the absolute path of the state directory (~/.cc-squash).
func StateDir() string {
	return daemon.StateDir()
}

// SocketPath is the daemon's control-plane unix socket.
func SocketPath() string {
	return daemon.SocketPath()
}

// PortFilePath is the file holding the daemon's listening port.
func PortFilePath() string {
	return path("daemon-v1.port")
}

// ProxySocketPath is the unix socket the Rust ccs-proxy data plane listens on.
func ProxySocketPath() string {
	return path("proxy-v1.sock")
}

// RefsDbPath is the SQLite database the Rust proxy opens for staged refs.
func RefsDbPath() string {
	return path("refs-v1.db")
}

// LogPath is the daemon's log file.
func LogPath() string {
	return daemon.LogPath()
}

// StatusPath is the out-of-process status mirror a status command reads.
func StatusPath() string {
	return path("status-v1.json")
}

// MCPConfigPath is the generated per-session Claude MCP configuration.
func MCPConfigPath() string {
	return path("mcp-v1.json")
}

// ConfigPath is the user's cc-squash configuration file.
func ConfigPath() string {
	return path("config.toml")
}

// LocksDir holds the daemon's lock files.
func LocksDir() string {
	return daemon.LockDir()
}

// StartLockPath serializes daemonkit cold-start and upgrade attempts.
func StartLockPath() string { return daemon.StartLockPath() }

// ProcessStorePath is daemonkit's durable process identity and receipt ledger.
func ProcessStorePath() string { return path("processes.db") }

// BinDir holds binaries the daemon manages (the ccs-proxy child).
func BinDir() string {
	return path("bin")
}

// EnsureStateDir creates the state directory (0700) if it does not exist.
func EnsureStateDir() error {
	return daemon.EnsureStateDir()
}

// EnsureLockDir creates daemonkit's launch serialization directory.
func EnsureLockDir() error { return daemon.EnsureLockDir() }
