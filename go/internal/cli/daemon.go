package cli

import (
	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
)

// newDaemonCmd is the hidden entry point launched by the LaunchAgent: it runs
// the control-plane daemon until signalled or told to step down over the socket.
func newDaemonCmd() *cobra.Command {
	return &cobra.Command{
		Use:    "daemon",
		Short:  "Run the background control-plane daemon used by the LaunchAgent",
		Hidden: true,
		Args:   cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			server, err := control.NewServer()
			if err != nil {
				return err
			}
			return server.Run(cmd.Context())
		},
	}
}
