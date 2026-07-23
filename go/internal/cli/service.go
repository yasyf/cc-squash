package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/daemonkit/service"
)

const (
	serviceWorkerLimit  = 1
	serviceCloseTimeout = 30 * time.Second
)

type serviceController interface {
	Converge(context.Context, []service.Agent) error
	Close(context.Context) error
}

type daemonRuntimeClient interface {
	Current(context.Context) bool
	WaitReady(context.Context, time.Duration) error
	WaitGone(context.Context, time.Duration) error
}

var openServiceController = func(ctx context.Context) (serviceController, error) {
	return service.NewController(ctx, service.ControllerConfig{
		StatePath:   paths.ServiceStatePath(),
		ProcessPath: paths.ServiceProcessStorePath(),
		WorkerLimit: serviceWorkerLimit,
	})
}

// ccsAgent is cc-squash's daemon LaunchAgent / brew-services descriptor: the
// generic launchctl + Homebrew lifecycle (daemonkit/service) configured with
// cc-squash's label, formula, daemon args, log path, HOME, and PATH. The program
// defaults to the running binary (os.Executable), so a Homebrew symlink stays a
// stable launchd program path across upgrades.
func ccsAgent() service.Agent {
	return service.Agent{
		Label:         control.DaemonRoleID,
		Args:          []string{"daemon"},
		LogPath:       paths.LogPath(),
		Env:           map[string]string{"HOME": os.Getenv("HOME"), "PATH": os.Getenv("PATH")},
		RestartPolicy: service.RestartAlways,
	}
}

func withServiceController(
	ctx context.Context,
	run func(serviceController) error,
) (err error) {
	controller, err := openServiceController(ctx)
	if err != nil {
		return err
	}
	defer func() {
		closeCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), serviceCloseTimeout)
		defer cancel()
		err = errors.Join(err, controller.Close(closeCtx))
	}()
	return run(controller)
}

func convergeService(ctx context.Context, agents []service.Agent) error {
	return withServiceController(ctx, func(controller serviceController) error {
		return controller.Converge(ctx, agents)
	})
}

func ensureDaemonCurrent(ctx context.Context, timeout time.Duration) error {
	client := control.NewClient()
	defer client.Close()
	return ensureDaemonCurrentWith(ctx, timeout, client)
}

func ensureDaemonCurrentWith(ctx context.Context, timeout time.Duration, client daemonRuntimeClient) error {
	if client.Current(ctx) {
		return nil
	}
	if err := convergeService(ctx, []service.Agent{ccsAgent()}); err != nil {
		return err
	}
	return client.WaitReady(ctx, timeout)
}

func installDaemonService(ctx context.Context, timeout time.Duration) error {
	client := control.NewClient()
	defer client.Close()
	return installDaemonServiceWith(ctx, timeout, client)
}

func installDaemonServiceWith(ctx context.Context, timeout time.Duration, client daemonRuntimeClient) error {
	if err := convergeService(ctx, []service.Agent{ccsAgent()}); err != nil {
		return err
	}
	return client.WaitReady(ctx, timeout)
}

func removeDaemonService(ctx context.Context, timeout time.Duration) error {
	client := control.NewClient()
	defer client.Close()
	return removeDaemonServiceWith(ctx, timeout, client)
}

func removeDaemonServiceWith(ctx context.Context, timeout time.Duration, client daemonRuntimeClient) error {
	if err := convergeService(ctx, nil); err != nil {
		return err
	}
	return client.WaitGone(ctx, timeout)
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
				if err := installDaemonService(cmd.Context(), proxyEnsureTimeout); err != nil {
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
				client := control.NewClient()
				defer client.Close()
				if health, err := client.RuntimeHealth(cmd.Context()); err == nil {
					_, _ = fmt.Fprintf(out, "Daemon: running (%s, %s)\n", health.Build, health.State)
				} else {
					_, _ = fmt.Fprintln(out, "Daemon: not responding")
				}
				_, _ = fmt.Fprintf(out, "Socket: %s\n", paths.SocketPath())
				return nil
			},
		},
	)
	return cmd
}
