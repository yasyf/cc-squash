// Package paths names every leaf of cc-squash's private state directory
// (~/.cc-squash), the single source of truth the daemon, CLI, and proxy share
// for socket, port-file, log, status, config, and lock locations. fusekit's
// state.Dir owns home resolution and the temp+rename atomic write; this package
// only fixes the App name and the leaf names so callers never repeat a literal.
package paths

import "github.com/yasyf/fusekit/state"

var stateDir = state.Dir{App: "cc-squash"}

// StateDir is the absolute path of the state directory (~/.cc-squash).
func StateDir() string {
	return stateDir.Root()
}

// SocketPath is the daemon's control-plane unix socket.
func SocketPath() string {
	return stateDir.Path("daemon.sock")
}

// PortFilePath is the file holding the daemon's listening port.
func PortFilePath() string {
	return stateDir.Path("daemon.port")
}

// ProxySocketPath is the unix socket the Rust ccs-proxy data plane listens on.
func ProxySocketPath() string {
	return stateDir.Path("proxy.sock")
}

// LogPath is the daemon's log file.
func LogPath() string {
	return stateDir.Path("daemon.log")
}

// StatusPath is the out-of-process status mirror a status command reads.
func StatusPath() string {
	return stateDir.Path("status.json")
}

// ConfigPath is the user's cc-squash configuration file.
func ConfigPath() string {
	return stateDir.Path("config.toml")
}

// LocksDir holds the daemon's lock files.
func LocksDir() string {
	return stateDir.Path("locks")
}

// BinDir holds binaries the daemon manages (the ccs-proxy child).
func BinDir() string {
	return stateDir.Path("bin")
}

// EnsureStateDir creates the state directory (0700) if it does not exist.
func EnsureStateDir() error {
	return stateDir.Ensure()
}
