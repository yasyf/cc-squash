package cli

import (
	"fmt"
	"log/slog"
	"os"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
)

const logLevelEnv = "CCS_LOG_LEVEL"

// newDaemonCmd is the hidden entry point launched by the LaunchAgent: it runs
// the control-plane daemon until signalled or told to step down over the socket.
func newDaemonCmd() *cobra.Command {
	return &cobra.Command{
		Use:    "daemon",
		Short:  "Run the background control-plane daemon used by the LaunchAgent",
		Hidden: true,
		Args:   cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			level, err := daemonLogLevel(os.Getenv(logLevelEnv))
			if err != nil {
				return err
			}
			slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level})))
			server, err := control.NewServer()
			if err != nil {
				return err
			}
			return server.Run(cmd.Context())
		},
	}
}

func daemonLogLevel(raw string) (slog.Level, error) {
	if raw == "" {
		return slog.LevelInfo, nil
	}
	var level slog.Level
	if err := level.UnmarshalText([]byte(raw)); err != nil {
		return 0, fmt.Errorf("parse %s: %w", logLevelEnv, err)
	}
	return level, nil
}
