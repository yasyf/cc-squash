// Command ccs is the cc-squash control-plane binary: it supervises the Rust
// ccs-proxy data plane and exposes the daemon/CLI surface (url, env, run,
// status, stop, logs, kill, shadow, gc, doctor, service, daemon). Layer 1 ships
// the foundation; the compaction engine lands in later layers.
package main

import (
	"fmt"
	"os"

	"github.com/yasyf/cc-squash/go/internal/cli"
	"github.com/yasyf/daemonkit/trust"
)

func main() {
	if handled, err := trust.RunVerifierChild(os.Args[1:], os.Stdout); handled {
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	os.Args = append(os.Args[:1], cli.InjectRun(os.Args[1:])...)
	if err := cli.NewRootCmd().Execute(); err != nil {
		fmt.Fprintln(os.Stderr, "ccs:", err)
		os.Exit(1)
	}
}
