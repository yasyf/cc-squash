package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
)

func newGCCmd() *cobra.Command {
	return &cobra.Command{
		Use:    "gc",
		Short:  "Garbage-collect stale session state",
		Hidden: true,
		Args:   cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			c := control.NewClient()
			defer c.Close()
			if err := c.EnsureCurrent(cmd.Context(), proxyEnsureTimeout); err != nil {
				return err
			}
			resp, err := c.Gc(cmd.Context())
			if err != nil {
				return err
			}
			if !resp.OK {
				return fmt.Errorf("gc failed: %s", resp.Error)
			}
			_, _ = fmt.Fprintln(cmd.ErrOrStderr(), "ccs gc: swept the proxy ref store to its reachable set")
			return nil
		},
	}
}
