package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

func newGCCmd() *cobra.Command {
	return &cobra.Command{
		Use:    "gc",
		Short:  "Garbage-collect stale session state",
		Hidden: true,
		Args:   cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			_, _ = fmt.Fprintln(cmd.ErrOrStderr(), "ccs gc: not yet implemented")
			return nil
		},
	}
}
