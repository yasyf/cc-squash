package cli

import (
	"context"
	"fmt"
	"time"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/daemonkit"
)

// daemonLauncher is the daemonkit convergence surface the service commands
// drive; tests substitute it for one that records the calls.
type daemonLauncher interface {
	Ensure(context.Context) error
	Stop(context.Context) error
	Health(context.Context) (daemonkit.Health, error)
	Close() error
}

var openDaemonLauncher = func() (daemonLauncher, error) { return control.NewClient() }

func withDaemonLauncher(ctx context.Context, timeout time.Duration, run func(context.Context, daemonLauncher) error) error {
	launcher, err := openDaemonLauncher()
	if err != nil {
		return err
	}
	defer launcher.Close()
	boundedCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	return run(boundedCtx, launcher)
}

// ensureDaemonCurrent makes the exact build of this executable serve, ready,
// cold-starting one when none runs and evicting a different incumbent.
func ensureDaemonCurrent(ctx context.Context, timeout time.Duration) error {
	return withDaemonLauncher(ctx, timeout, func(ctx context.Context, launcher daemonLauncher) error {
		return launcher.Ensure(ctx)
	})
}

// removeDaemonService drains the incumbent and removes its LaunchAgent.
func removeDaemonService(ctx context.Context, timeout time.Duration) error {
	return withDaemonLauncher(ctx, timeout, func(ctx context.Context, launcher daemonLauncher) error {
		return launcher.Stop(ctx)
	})
}

func newServiceCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "service",
		Short: "Manage the background daemon",
	}
	cmd.AddCommand(
		&cobra.Command{
			Use:   "install",
			Short: "Install and start the user LaunchAgent",
			Args:  cobra.NoArgs,
			RunE: func(cmd *cobra.Command, _ []string) error {
				if err := ensureDaemonCurrent(cmd.Context(), proxyEnsureTimeout); err != nil {
					return err
				}
				_, _ = fmt.Fprintln(cmd.OutOrStdout(), "Installed and started the daemon.")
				return nil
			},
		},
		&cobra.Command{
			Use:   "uninstall",
			Short: "Stop the daemon and remove the LaunchAgent",
			Args:  cobra.NoArgs,
			RunE: func(cmd *cobra.Command, _ []string) error {
				if err := removeDaemonService(cmd.Context(), stopGoneWait); err != nil {
					return err
				}
				_, _ = fmt.Fprintln(cmd.OutOrStdout(), "Removed the LaunchAgent.")
				return nil
			},
		},
		&cobra.Command{
			Use:   "status",
			Short: "Show daemon/LaunchAgent status",
			Args:  cobra.NoArgs,
			RunE: func(cmd *cobra.Command, _ []string) error {
				out := cmd.OutOrStdout()
				err := withDaemonLauncher(cmd.Context(), healthTimeout, func(ctx context.Context, launcher daemonLauncher) error {
					health, err := launcher.Health(ctx)
					if err != nil {
						_, _ = fmt.Fprintln(out, "Daemon: not responding")
						return nil
					}
					build, _ := control.ReportedBuild(health)
					_, _ = fmt.Fprintf(out, "Daemon: running (%s, %s)\n", build, phaseName(health.Phase))
					return nil
				})
				if err != nil {
					return err
				}
				socket, err := control.SocketPath()
				if err != nil {
					return err
				}
				_, _ = fmt.Fprintf(out, "Socket: %s\n", socket)
				return nil
			},
		},
	)
	return cmd
}

// healthTimeout bounds one control-lane health read.
const healthTimeout = 5 * time.Second

func phaseName(phase daemonkit.Phase) string {
	switch phase {
	case daemonkit.PhaseStarting:
		return "starting"
	case daemonkit.PhaseReady:
		return "ready"
	case daemonkit.PhaseDraining:
		return "draining"
	case daemonkit.PhaseFailed:
		return "failed"
	default:
		return "unknown"
	}
}
