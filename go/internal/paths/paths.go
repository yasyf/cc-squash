// Package paths names every leaf of cc-squash's private state directory
// (~/.cc-squash), the single source of truth the daemon, CLI, and proxy share
// for socket, port-file, log, status, config, and lock locations.
package paths

import (
	"fmt"
	"os"
	"path/filepath"
)

func stateDir() string {
	home, err := os.UserHomeDir()
	if err != nil {
		panic(fmt.Errorf("resolve home dir: %w", err))
	}
	return filepath.Join(home, ".cc-squash")
}

func path(leaf string) string {
	return filepath.Join(stateDir(), leaf)
}

// StateDir is the absolute path of the state directory (~/.cc-squash).
func StateDir() string {
	return stateDir()
}

// SocketPath is the daemon's control-plane unix socket.
func SocketPath() string {
	return path("daemon.sock")
}

// PortFilePath is the file holding the daemon's listening port.
func PortFilePath() string {
	return path("daemon.port")
}

// ProxySocketPath is the unix socket the Rust ccs-proxy data plane listens on.
func ProxySocketPath() string {
	return path("proxy.sock")
}

// RefsDbPath is the SQLite database the Rust proxy opens for staged refs.
func RefsDbPath() string {
	return path("refs.db")
}

// LogPath is the daemon's log file.
func LogPath() string {
	return path("daemon.log")
}

// StatusPath is the out-of-process status mirror a status command reads.
func StatusPath() string {
	return path("status.json")
}

// ConfigPath is the user's cc-squash configuration file.
func ConfigPath() string {
	return path("config.toml")
}

// LocksDir holds the daemon's lock files.
func LocksDir() string {
	return path("locks")
}

// BinDir holds binaries the daemon manages (the ccs-proxy child).
func BinDir() string {
	return path("bin")
}

// EnsureStateDir creates the state directory (0700) if it does not exist.
func EnsureStateDir() error {
	return os.MkdirAll(stateDir(), 0o700)
}
