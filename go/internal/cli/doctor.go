package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/version"
)

func newDoctorCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "doctor",
		Short: "Run a daemon-health self-test",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			out := cmd.OutOrStdout()
			_, _ = fmt.Fprintf(out, "ccs %s\n", version.String())
			_, _ = fmt.Fprintf(out, "socket: %s\n", paths.SocketPath())
			client := control.NewClient()
			defer client.Close()
			health, err := client.RuntimeHealth(cmd.Context())
			switch {
			case err != nil:
				_, _ = fmt.Fprintf(out, "daemon: not responding (%v)\n", err)
			case health.RuntimeBuild != version.String():
				_, _ = fmt.Fprintf(out, "daemon: running, version skew (%s)\n", health.RuntimeBuild)
			case health.Draining:
				_, _ = fmt.Fprintf(out, "daemon: draining (%s)\n", health.RuntimeBuild)
			default:
				_, _ = fmt.Fprintf(out, "daemon: %s (%s)\n", health.State, health.RuntimeBuild)
			}
			return nil
		},
	}
}
