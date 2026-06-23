// Command ccs is the cc-squash control-plane binary: it supervises the Rust
// ccs-proxy data plane and exposes the daemon/CLI surface (url, env, run,
// status, stop, logs, kill, service, daemon). Layer 1 ships the foundation;
// the compaction engine lands in later layers.
package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"

	"github.com/yasyf/cc-squash/go/internal/version"
)

func newRootCmd() *cobra.Command {
	return &cobra.Command{
		Use:           "ccs",
		Short:         "cc-squash control plane",
		Version:       version.String(),
		SilenceUsage:  true,
		SilenceErrors: true,
	}
}

func main() {
	if err := newRootCmd().Execute(); err != nil {
		fmt.Fprintln(os.Stderr, "ccs:", err)
		os.Exit(1)
	}
}
