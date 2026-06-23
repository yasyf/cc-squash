package cli

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/fusekit/service"
)

// ccsAgent is cc-squash's daemon LaunchAgent / brew-services descriptor: the
// generic launchctl + Homebrew lifecycle (fusekit/service) configured with
// cc-squash's label, formula, daemon args, log path, and PATH. The program
// defaults to the running binary (os.Executable), so a Homebrew symlink stays a
// stable launchd program path across upgrades.
func ccsAgent() service.Agent {
	return service.Agent{
		Label:   "com.yasyf.cc-squash",
		Formula: "cc-squash",
		Args:    []string{"daemon"},
		LogPath: paths.LogPath(),
		Env:     map[string]string{"PATH": os.Getenv("PATH")},
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
				if err := ccsAgent().Install(); err != nil {
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
				if err := ccsAgent().Uninstall(); err != nil {
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
				for _, line := range ccsAgent().StatusLines() {
					_, _ = fmt.Fprintln(out, line)
				}
				if resp, err := control.NewClient().Health(); err == nil && resp.OK {
					_, _ = fmt.Fprintf(out, "Daemon: running (%s)\n", resp.Version)
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
