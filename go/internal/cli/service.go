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
	"github.com/yasyf/cc-squash/go/internal/version"
	"github.com/yasyf/daemonkit/service"
	"github.com/yasyf/daemonkit/wire"
)

const (
	serviceWorkerLimit  = 1
	serviceCloseTimeout = 30 * time.Second
	stopRuntimeCommand  = "__stop-runtime"
)

type serviceController interface {
	Converge(context.Context, []service.Agent) error
	StopRuntime(context.Context, service.StopControlSpec) (wire.StopResult, error)
	Close(context.Context) error
}

type daemonRuntimeClient interface {
	Current(context.Context) bool
	RuntimeHealth(context.Context) (control.RuntimeHealth, error)
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
// cc-squash's label, formula, daemon args, log path, HOME, and PATH.
func ccsAgent() (service.Agent, error) {
	program, err := service.CanonicalExecutable()
	if err != nil {
		return service.Agent{}, err
	}
	return service.Agent{
		Label:         control.DaemonRoleID,
		Program:       program,
		Args:          []string{"daemon"},
		LogPath:       paths.LogPath(),
		Env:           map[string]string{"HOME": os.Getenv("HOME"), "PATH": os.Getenv("PATH")},
		RestartPolicy: service.RestartAlways,
	}, nil
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
	current := client.Current(ctx)
	if current {
		return nil
	}
	if err := convergeDaemonService(ctx, client, current); err != nil {
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
	if err := convergeDaemonService(ctx, client, client.Current(ctx)); err != nil {
		return err
	}
	return client.WaitReady(ctx, timeout)
}

func convergeDaemonService(ctx context.Context, client daemonRuntimeClient, current bool) error {
	agent, err := ccsAgent()
	if err != nil {
		return err
	}
	var observed *control.RuntimeHealth
	if !current {
		health, err := client.RuntimeHealth(ctx)
		if err == nil {
			observed = &health
		}
	}
	return withServiceController(ctx, func(controller serviceController) error {
		if observed != nil {
			spec, err := daemonStopSpec(*observed)
			if err != nil {
				return err
			}
			if _, err := controller.StopRuntime(ctx, spec); err != nil {
				return err
			}
		}
		return controller.Converge(ctx, []service.Agent{agent})
	})
}

func daemonStopSpec(health control.RuntimeHealth) (service.StopControlSpec, error) {
	executable, err := service.CanonicalExecutable()
	if err != nil {
		return service.StopControlSpec{}, err
	}
	intent := wire.StopIntentRestart
	if health.RuntimeBuild != version.String() {
		intent = wire.StopIntentUpgrade
	}
	return service.StopControlSpec{
		Executable: executable, Args: []string{stopRuntimeCommand},
		Role: control.StopControlRoleID, RuntimeBuild: version.String(),
		RuntimeProtocol: int(wire.ProtocolVersion), TargetProcessGeneration: health.ProcessGeneration,
		Intent: intent,
	}, nil
}

func newStopRuntimeCmd() *cobra.Command {
	return &cobra.Command{
		Use: stopRuntimeCommand, Hidden: true, Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			_, err := service.RunStopControlChild(cmd.Context(), service.StopControlClientConfig{
				Dial: wire.UnixDialer(paths.SocketPath()), WireBuild: control.WireBuild,
				RuntimeProtocol: int(wire.ProtocolVersion),
			})
			return err
		},
	}
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
					_, _ = fmt.Fprintf(out, "Daemon: running (%s, %s)\n", health.RuntimeBuild, health.State)
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
