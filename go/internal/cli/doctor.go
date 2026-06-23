package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/fusekit/version"
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
			resp, err := control.NewClient().Health()
			switch {
			case err != nil:
				_, _ = fmt.Fprintf(out, "daemon: not responding (%v)\n", err)
			case !resp.OK:
				_, _ = fmt.Fprintln(out, "daemon: unhealthy")
			case resp.Version != version.String():
				_, _ = fmt.Fprintf(out, "daemon: running, version skew (%s)\n", resp.Version)
			default:
				_, _ = fmt.Fprintf(out, "daemon: healthy (%s)\n", resp.Version)
			}
			return nil
		},
	}
}
