package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
)

func newShadowCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "shadow on|off",
		Short: "Toggle the proxy's shadow mode",
		Long: `shadow toggles the proxy's shadow mode: with it on the proxy computes the
compacted transcript and logs what it would do without altering the live
request, so you can audit compaction against real traffic before trusting it.

    ccs shadow on
    ccs shadow off`,
		Args:      cobra.ExactArgs(1),
		ValidArgs: []string{"on", "off"},
		RunE: func(cmd *cobra.Command, args []string) error {
			on, err := parseToggle(args[0])
			if err != nil {
				return err
			}
			resp, err := control.NewClient().Shadow(on)
			if err != nil {
				return err
			}
			_, _ = fmt.Fprintf(cmd.OutOrStdout(), "shadow: %s\n", onOff(resp.Shadow))
			return nil
		},
	}
}
