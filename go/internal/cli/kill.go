package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
)

func newKillCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "kill on|off|status",
		Short: "Toggle the proxy kill switch (off-rails passthrough)",
		Long: `kill toggles the proxy's kill switch. With it on the proxy passes every request
straight through untouched; with it off cc-squash's compaction is active.

    ccs kill on      # disable compaction, raw passthrough
    ccs kill off     # re-enable compaction
    ccs kill status  # report the current setting`,
		Args:      cobra.ExactArgs(1),
		ValidArgs: []string{"on", "off", "status"},
		RunE: func(cmd *cobra.Command, args []string) error {
			if args[0] == "status" {
				on, err := readToggle(func(s control.StatusSnapshot) bool { return s.Kill })
				if err != nil {
					return err
				}
				_, _ = fmt.Fprintf(cmd.OutOrStdout(), "kill: %s\n", onOff(on))
				return nil
			}
			on, err := parseToggle(args[0])
			if err != nil {
				return err
			}
			resp, err := control.NewClient().Kill(on)
			if err != nil {
				return err
			}
			_, _ = fmt.Fprintf(cmd.OutOrStdout(), "kill: %s\n", onOff(resp.Kill))
			return nil
		},
	}
}

// parseToggle reads an on/off argument, erroring on anything else.
func parseToggle(arg string) (bool, error) {
	switch arg {
	case "on":
		return true, nil
	case "off":
		return false, nil
	default:
		return false, fmt.Errorf("expected on or off, got %q", arg)
	}
}

// readToggle reads one toggle's value from the daemon's published status
// snapshot via field.
func readToggle(field func(control.StatusSnapshot) bool) (bool, error) {
	snap, err := control.ReadStatus()
	if err != nil {
		return false, err
	}
	return field(snap), nil
}
