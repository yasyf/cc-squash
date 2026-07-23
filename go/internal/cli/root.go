// Package cli wires up the cobra command tree for cc-squash. The binary is
// installed as `ccs`; main dispatches here after InjectRun rewrites a
// claude-flag-led invocation into `ccs run`.
package cli

import (
	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/version"
)

// NewRootCmd builds the root command and attaches every subcommand. Bare `ccs`
// prints the status table when a daemon has published one, else the help text.
func NewRootCmd() *cobra.Command {
	root := &cobra.Command{
		Use:   "ccs",
		Short: "Augmented auto-compaction control plane for Claude Code",
		Long: `cc-squash (ccs) supervises the ccs-proxy data plane and points Claude Code at
it, so long-running sessions compact continuously:

    eval "$(ccs env)"; claude

Run ` + "`ccs run`" + ` to launch claude through the proxy directly, or ` + "`ccs url`" + ` to mint
a session URL. Bare ` + "`ccs`" + ` shows the daemon status.`,
		Version:       version.String(),
		SilenceUsage:  true,
		SilenceErrors: true,
		// Args left nil: cobra's legacyArgs rejects unknown subcommands on a root
		// with children (with suggestions), so RunE only runs for bare `ccs`.
		RunE: func(cmd *cobra.Command, _ []string) error {
			return runStatus(cmd, false)
		},
	}
	root.SetVersionTemplate("{{.Version}}\n")

	root.AddCommand(
		newURLCmd(),
		newEnvCmd(),
		newRunCmd(),
		newStatusCmd(),
		newStopCmd(),
		newLogsCmd(),
		newKillCmd(),
		newShadowCmd(),
		newGCCmd(),
		newDoctorCmd(),
		newServiceCmd(),
		newDaemonCmd(),
		newStopRuntimeCmd(),
	)
	return root
}
