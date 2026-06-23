package cli

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"text/tabwriter"
	"time"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
)

func newStatusCmd() *cobra.Command {
	var jsonOut bool
	cmd := &cobra.Command{
		Use:   "status",
		Short: "Show the daemon and proxy status",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			return runStatus(cmd, jsonOut)
		},
	}
	cmd.Flags().BoolVar(&jsonOut, "json", false, "print the status snapshot JSON (same schema as ~/.cc-squash/status.json)")
	return cmd
}

// runStatus prints the daemon's last-published status: the snapshot JSON under
// --json, else a plain table. A daemon that has never published a snapshot
// (cold, or never started) prints a one-line "not running" notice instead of an
// error, so bare `ccs` on a fresh machine is informative rather than failing.
func runStatus(cmd *cobra.Command, jsonOut bool) error {
	snap, err := control.ReadStatus()
	if errors.Is(err, fs.ErrNotExist) {
		_, _ = fmt.Fprintln(cmd.ErrOrStderr(), "cc-squash daemon not running. Run `ccs url` or `ccs run` to start it.")
		return nil
	}
	if err != nil {
		return err
	}
	if jsonOut {
		out, err := json.MarshalIndent(snap, "", "  ")
		if err != nil {
			return fmt.Errorf("encode status snapshot: %w", err)
		}
		_, _ = fmt.Fprintln(cmd.OutOrStdout(), string(out))
		return nil
	}
	return renderStatus(cmd, snap)
}

// renderStatus prints the status snapshot as a plain aligned table.
func renderStatus(cmd *cobra.Command, snap control.StatusSnapshot) error {
	tw := tabwriter.NewWriter(cmd.OutOrStdout(), 0, 2, 2, ' ', 0)
	_, _ = fmt.Fprintf(tw, "VERSION\t%s\n", snap.Version)
	_, _ = fmt.Fprintf(tw, "PROXY PORT\t%d\n", snap.ProxyPort)
	_, _ = fmt.Fprintf(tw, "PROXY PID\t%d\n", snap.ProxyPID)
	_, _ = fmt.Fprintf(tw, "SESSIONS\t%d\n", snap.Sessions)
	_, _ = fmt.Fprintf(tw, "KILL\t%s\n", onOff(snap.Kill))
	_, _ = fmt.Fprintf(tw, "SHADOW\t%s\n", onOff(snap.Shadow))
	_, _ = fmt.Fprintf(tw, "SOCKET\t%s\n", paths.SocketPath())
	_, _ = fmt.Fprintf(tw, "UPDATED\t%s\n", snap.GeneratedAt.Local().Format(time.Kitchen))
	return tw.Flush()
}

// onOff renders a toggle as "on"/"off" for the status table.
func onOff(on bool) string {
	if on {
		return "on"
	}
	return "off"
}
