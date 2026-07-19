// Package version owns cc-squash's build stamp and delegates build
// classification to daemonkit.
package version

import dkversion "github.com/yasyf/daemonkit/version"

var (
	// Version is set by release builds through -ldflags.
	Version = "dev"
	// Commit is set by release builds through -ldflags.
	Commit = ""
)

// String returns the running build's display version.
func String() string {
	v := dkversion.Running(Version)
	if Commit != "" {
		v += " (" + Commit + ")"
	}
	return v
}
