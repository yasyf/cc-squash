package cli

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"syscall"

	"github.com/spf13/cobra"
)

func newRunCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "run [claude args...]",
		Short: "Mint a session URL and exec `claude` through the proxy, passing every arg through",
		Long: `run mints a session URL, bakes the proxy env into the environment, and replaces
itself with ` + "`claude`" + ` (via exec), so once claude starts cc-squash is gone from the
process tree — signals, the controlling terminal, and the exit code are all claude's.

Every argument is forwarded verbatim, with no ` + "`--`" + ` separator (e.g. ` + "`ccs run --resume`" + `).
This is the imperative equivalent of:

    eval "$(ccs env)"; claude ...`,
		// Pass every argument straight through to claude; ccs owns no flags here.
		DisableFlagParsing: true,
		RunE: func(cmd *cobra.Command, args []string) error {
			url, err := resolveURL()
			if err != nil {
				return err
			}
			return execClaude(url, args)
		},
	}
}

// execClaude replaces this process with `claude`, forwarding args verbatim and
// baking the proxy env into its environment. It does not return on success.
func execClaude(url string, args []string) error {
	bin, err := exec.LookPath("claude")
	if err != nil {
		return fmt.Errorf("`claude` not found on PATH: %w", err)
	}
	argv := append([]string{"claude"}, args...)
	//nolint:gosec // G204: bin is the resolved claude executable; argv are this CLI's own passthrough args
	if err := syscall.Exec(bin, argv, proxyEnv(os.Environ(), url)); err != nil {
		return fmt.Errorf("exec claude: %w", err)
	}
	return nil // unreachable: a successful Exec never returns
}

// proxyEnv returns environ with any existing ANTHROPIC_BASE_URL dropped and the
// three proxy exports appended, so the launched claude sees exactly one base URL
// (a duplicate key has platform-dependent getenv precedence).
func proxyEnv(environ []string, url string) []string {
	out := make([]string, 0, len(environ)+3)
	for _, e := range environ {
		if strings.HasPrefix(e, baseURLEnv+"=") {
			continue
		}
		out = append(out, e)
	}
	return append(out,
		baseURLEnv+"="+url,
		toolSearchEnv+"="+toolSearchValue,
		firstPartyEnv+"="+firstPartyValue,
	)
}
