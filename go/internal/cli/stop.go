package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
)

// stopGoneWait bounds the wait for the daemon to release its socket after being
// told to step down.
const stopGoneWait = 5 * time.Second

func newStopCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "stop",
		Short: "Stop the running daemon and release its socket",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			c := control.NewClient()
			if !c.Available() {
				_, _ = fmt.Fprintln(cmd.ErrOrStderr(), "cc-squash daemon not running.")
				return nil
			}
			if _, err := c.Shutdown(); err != nil {
				return err
			}
			if !c.WaitGone(stopGoneWait) {
				return fmt.Errorf("daemon did not release %s in time", paths.SocketPath())
			}
			_, _ = fmt.Fprintln(cmd.OutOrStdout(), "Stopped the daemon.")
			return nil
		},
	}
}
