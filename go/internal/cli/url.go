package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
)

// proxyEnsureTimeout bounds the wait for an auto-spawned daemon's control socket
// to come up before a mint.
const proxyEnsureTimeout = 10 * time.Second

// Environment cc-squash sets so Claude Code routes through the proxy: the base
// URL points at the minted session, tool search is on, and the first-party
// assumption keeps Claude from rejecting a non-anthropic.com base.
const (
	baseURLEnv      = "ANTHROPIC_BASE_URL"
	toolSearchEnv   = "ENABLE_TOOL_SEARCH"
	firstPartyEnv   = "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL"
	toolSearchValue = "true"
	firstPartyValue = "1"
)

func newURLCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "url",
		Short: "Mint a session URL for the proxy and print it",
		Long: `url ensures the daemon is running, mints a fresh session token, and prints the
proxy URL claude should use as its base. Only the URL goes to stdout; every
diagnostic goes to stderr, so ` + "`ANTHROPIC_BASE_URL=$(ccs url)`" + ` captures just the URL.`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			url, err := resolveURL()
			if err != nil {
				return err
			}
			_, _ = fmt.Fprintln(cmd.OutOrStdout(), url)
			return nil
		},
	}
}

// resolveURL ensures the daemon is up, mints a session token, and returns the
// proxy base URL the minted session answers at.
func resolveURL() (string, error) {
	c := control.NewClient()
	if !c.EnsureRunning(proxyEnsureTimeout) {
		return "", control.ErrDaemonUnavailable
	}
	resp, err := c.Mint()
	if err != nil {
		return "", err
	}
	if !resp.OK {
		return "", fmt.Errorf("mint failed: %s", resp.Error)
	}
	return fmt.Sprintf("http://127.0.0.1:%d/s/%s", resp.Port, resp.Token), nil
}
