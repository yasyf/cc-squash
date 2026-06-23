package cli

import (
	"fmt"
	"io"
	"os"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/paths"
)

func newLogsCmd() *cobra.Command {
	var pathOnly bool
	cmd := &cobra.Command{
		Use:   "logs",
		Short: "Print the daemon log, or its path with --path",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			if pathOnly {
				_, _ = fmt.Fprintln(cmd.OutOrStdout(), paths.LogPath())
				return nil
			}
			f, err := os.Open(paths.LogPath())
			if err != nil {
				return err
			}
			defer func() { _ = f.Close() }()
			_, err = io.Copy(cmd.OutOrStdout(), f)
			return err
		},
	}
	cmd.Flags().BoolVar(&pathOnly, "path", false, "print the log file path instead of its contents")
	return cmd
}
