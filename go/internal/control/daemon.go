package control

import (
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/daemonkit"
	dkpaths "github.com/yasyf/daemonkit/paths"
)

// DaemonRoleID is the exact launchd label and daemonkit identity of the daemon.
const DaemonRoleID daemonkit.Label = "com.yasyf.cc-squash.daemon"

const (
	releaseTeamID            = "SXKCTF23Q2"
	releaseSigningIdentifier = "ccs"
)

const (
	daemonShutdown  = daemonkit.Grace(30 * time.Second)
	daemonHandshake = daemonkit.Grace(10 * time.Second)
)

// Identity is the daemon value every client reaches cc-squash by. It states no
// Program: only Ensure places an executable, and Stop renders no LaunchAgent.
func Identity() daemonkit.Daemon {
	control := daemonkit.Requirement{TeamID: releaseTeamID, SigningIdentifier: releaseSigningIdentifier}
	return daemonkit.Daemon{
		Label:   DaemonRoleID,
		Args:    []string{"daemon"},
		Schemas: []daemonkit.Schema{daemonkit.Schema(WireBuild)},
		Trust: daemonkit.Trust{
			Control:  &control,
			Business: nil,
			Serving:  daemonkit.ServingSameUser(),
		},
		Log:       paths.LogPath(),
		Restart:   daemonkit.RestartAlways,
		Shutdown:  daemonShutdown,
		Handshake: daemonHandshake,
	}
}

// Spec is Identity plus the executable launchd runs — a copy of the invoking
// binary Ensure places at daemonkit's stable path. Serving and converging both
// read it; every other client verb takes Identity.
func Spec() (daemonkit.Daemon, error) {
	program, err := daemonkit.Stable()
	if err != nil {
		return daemonkit.Daemon{}, err
	}
	d := Identity()
	d.Program = program
	return d, nil
}

// SocketPath is where daemonkit serves this daemon's control socket.
func SocketPath() (string, error) {
	return dkpaths.Socket(string(DaemonRoleID))
}
