package cli

import (
	"context"
	"fmt"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/version"
	"github.com/yasyf/daemonkit"
)

func newDoctorCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "doctor",
		Short: "Run a daemon-health self-test",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			out := cmd.OutOrStdout()
			_, _ = fmt.Fprintf(out, "ccs %s\n", version.String())
			socket, err := control.SocketPath()
			if err != nil {
				return err
			}
			_, _ = fmt.Fprintf(out, "socket: %s\n", socket)
			return withDaemonLauncher(cmd.Context(), healthTimeout, func(ctx context.Context, launcher daemonLauncher) error {
				health, err := launcher.Health(ctx)
				build, _ := control.ReportedBuild(health)
				switch {
				case err != nil:
					_, _ = fmt.Fprintf(out, "daemon: not responding (%v)\n", err)
				case build != version.String():
					_, _ = fmt.Fprintf(out, "daemon: running, version skew (%s)\n", build)
				case health.Phase == daemonkit.PhaseDraining:
					_, _ = fmt.Fprintf(out, "daemon: draining (%s)\n", build)
				default:
					_, _ = fmt.Fprintf(out, "daemon: %s (%s)\n", phaseName(health.Phase), build)
				}
				return nil
			})
		},
	}
}
