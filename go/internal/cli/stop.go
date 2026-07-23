package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
)

// stopGoneWait bounds the wait for the daemon to release its socket after being
// told to step down.
const stopGoneWait = 5 * time.Second

func newStopCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "stop",
		Short: "Stop the daemon and remove its user LaunchAgent",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			if err := removeDaemonService(cmd.Context(), stopGoneWait); err != nil {
				return fmt.Errorf("stop daemon service: %w", err)
			}
			_, _ = fmt.Fprintln(cmd.OutOrStdout(), "Stopped the daemon.")
			return nil
		},
	}
}
