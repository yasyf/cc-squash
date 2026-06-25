package cli

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
)

func newEnvCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "env",
		Short: "Print eval-able exports that point claude at the proxy",
		Long: `env mints a session URL and prints the exports claude needs to route through the
proxy:

    eval "$(ccs env)"; claude --mcp-config "$CC_SQUASH_MCP_CONFIG"

env cannot inject ` + "`claude`" + ` argv, so it cannot register the cc_squash_retrieve MCP
server the way ` + "`ccs run`" + ` does. Instead it writes the server config to a file and
exports CC_SQUASH_MCP_CONFIG pointing at it; pass that file to ` + "`claude --mcp-config`" + `
to enable retrieve, or use ` + "`ccs run`" + ` which wires it automatically.`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			resp, err := resolveMint()
			if err != nil {
				return err
			}
			cfgPath, err := writeMCPConfig(resp)
			if err != nil {
				return err
			}
			out := cmd.OutOrStdout()
			_, _ = fmt.Fprintf(out, "export %s=%s\n", baseURLEnv, sessionURL(resp))
			_, _ = fmt.Fprintf(out, "export %s=%s\n", toolSearchEnv, toolSearchValue)
			_, _ = fmt.Fprintf(out, "export %s=%s\n", firstPartyEnv, firstPartyValue)
			_, _ = fmt.Fprintf(out, "export %s=%s\n", mcpConfigEnv, cfgPath)
			_, _ = fmt.Fprintf(out, "# pass the MCP config to claude for cc_squash_retrieve: claude --mcp-config \"$%s\"\n", mcpConfigEnv)
			return nil
		},
	}
}

// writeMCPConfig renders the per-session cc-squash MCP server config and writes
// it to paths.StateDir()/mcp.json, returning the path. `ccs env` points
// CC_SQUASH_MCP_CONFIG at it so the user can hand it to `claude --mcp-config`.
func writeMCPConfig(resp control.Response) (string, error) {
	cfg, err := mcpConfigJSON(mcpURL(resp))
	if err != nil {
		return "", err
	}
	path := paths.StateDir() + "/mcp.json"
	if err := os.WriteFile(path, []byte(cfg+"\n"), 0o600); err != nil {
		return "", err
	}
	return path, nil
}
