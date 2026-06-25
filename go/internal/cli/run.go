package cli

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"syscall"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
)

// mcpServerName is the key cc-squash registers its retrieve MCP server under in
// the --mcp-config blob, so the tool surfaces to claude as cc-squash's.
const mcpServerName = "cc-squash"

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
			resp, err := resolveMint()
			if err != nil {
				return err
			}
			return execClaude(resp, args)
		},
	}
}

// execClaude replaces this process with `claude`, baking the proxy env into its
// environment and prepending the cc-squash retrieve MCP server to its argv (so
// `cc_squash_retrieve` reaches the live session), then forwarding the user's
// args verbatim. It does not return on success.
func execClaude(resp control.Response, args []string) error {
	bin, err := exec.LookPath("claude")
	if err != nil {
		return fmt.Errorf("`claude` not found on PATH: %w", err)
	}
	argv, err := claudeArgv(resp, args)
	if err != nil {
		return err
	}
	//nolint:gosec // G204: bin is the resolved claude executable; argv are this CLI's own passthrough args
	if err := syscall.Exec(bin, argv, proxyEnv(os.Environ(), sessionURL(resp))); err != nil {
		return fmt.Errorf("exec claude: %w", err)
	}
	return nil // unreachable: a successful Exec never returns
}

// claudeArgv assembles the claude argv: the cc-squash retrieve MCP server is
// prepended as `--mcp-config <json>` ahead of the user's args, so retrieve is
// registered before claude parses anything the user passed.
func claudeArgv(resp control.Response, args []string) ([]string, error) {
	cfg, err := mcpConfigJSON(mcpURL(resp))
	if err != nil {
		return nil, fmt.Errorf("build --mcp-config: %w", err)
	}
	return append([]string{"claude", "--mcp-config", cfg}, args...), nil
}

// mcpConfigJSON renders the single-server --mcp-config blob claude consumes:
// {"mcpServers":{"cc-squash":{"type":"http","url":"<mcpURL>"}}}. Built through
// encoding/json so the per-invocation URL is escaped correctly.
func mcpConfigJSON(mcpURL string) (string, error) {
	type httpServer struct {
		Type string `json:"type"`
		URL  string `json:"url"`
	}
	blob, err := json.Marshal(struct {
		Servers map[string]httpServer `json:"mcpServers"`
	}{Servers: map[string]httpServer{mcpServerName: {Type: "http", URL: mcpURL}}})
	if err != nil {
		return "", err
	}
	return string(blob), nil
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
