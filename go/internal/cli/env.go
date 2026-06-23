package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

func newEnvCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "env",
		Short: "Print eval-able exports that point claude at the proxy",
		Long: `env mints a session URL and prints the exports claude needs to route through the
proxy:

    eval "$(ccs env)"; claude`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			url, err := resolveURL()
			if err != nil {
				return err
			}
			out := cmd.OutOrStdout()
			_, _ = fmt.Fprintf(out, "export %s=%s\n", baseURLEnv, url)
			_, _ = fmt.Fprintf(out, "export %s=%s\n", toolSearchEnv, toolSearchValue)
			_, _ = fmt.Fprintf(out, "export %s=%s\n", firstPartyEnv, firstPartyValue)
			return nil
		},
	}
}
