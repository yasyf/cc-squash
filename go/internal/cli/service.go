package cli

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/daemonkit/service"
)

// ccsAgent is cc-squash's daemon LaunchAgent / brew-services descriptor: the
// generic launchctl + Homebrew lifecycle (daemonkit/service) configured with
// cc-squash's label, formula, daemon args, log path, and PATH. The program
// defaults to the running binary (os.Executable), so a Homebrew symlink stays a
// stable launchd program path across upgrades.
func ccsAgent() service.Agent {
	return service.Agent{
		Label:         control.DaemonRoleID,
		Formula:       "cc-squash",
		Args:          []string{"daemon"},
		LogPath:       paths.LogPath(),
		Env:           map[string]string{"PATH": os.Getenv("PATH")},
		RestartPolicy: service.RestartAlways,
	}
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
				if err := ccsAgent().Install(cmd.Context()); err != nil {
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
				if err := ccsAgent().Uninstall(cmd.Context()); err != nil {
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
				for _, line := range ccsAgent().StatusLines(cmd.Context()) {
					_, _ = fmt.Fprintln(out, line)
				}
				client := control.NewClient()
				defer client.Close()
				if health, err := client.Health(cmd.Context()); err == nil {
					_, _ = fmt.Fprintf(out, "Daemon: running (%s)\n", health.Build)
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
