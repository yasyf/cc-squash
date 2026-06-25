package cli

import (
	"slices"
	"testing"

	"github.com/yasyf/cc-squash/go/internal/control"
)

func TestProxyEnvBakesProxyVars(t *testing.T) {
	in := []string{"PATH=/usr/bin", "ANTHROPIC_BASE_URL=https://stale.example", "HOME=/home/x"}
	got := proxyEnv(in, "http://127.0.0.1:8080/s/tok")

	want := []string{
		"PATH=/usr/bin",
		"HOME=/home/x",
		"ANTHROPIC_BASE_URL=http://127.0.0.1:8080/s/tok",
		"ENABLE_TOOL_SEARCH=true",
		"_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1",
	}
	if !slices.Equal(got, want) {
		t.Fatalf("proxyEnv = %q, want %q", got, want)
	}

	// Exactly one ANTHROPIC_BASE_URL survives: the stale one was dropped.
	n := 0
	for _, e := range got {
		if len(e) >= len(baseURLEnv)+1 && e[:len(baseURLEnv)+1] == baseURLEnv+"=" {
			n++
		}
	}
	if n != 1 {
		t.Fatalf("got %d ANTHROPIC_BASE_URL entries, want exactly 1", n)
	}
}

// TestMCPConfigJSON pins the exact --mcp-config blob ccs run injects: a single
// http server keyed "cc-squash" pointing at the per-session mcpPort+token /mcp
// endpoint (the rmcp cc_squash_retrieve listener).
func TestMCPConfigJSON(t *testing.T) {
	resp := control.Response{MCPPort: 50517, Token: "tok-abc"}
	got, err := mcpConfigJSON(mcpURL(resp))
	if err != nil {
		t.Fatalf("mcpConfigJSON: %v", err)
	}
	want := `{"mcpServers":{"cc-squash":{"type":"http","url":"http://127.0.0.1:50517/s/tok-abc/mcp"}}}`
	if got != want {
		t.Fatalf("mcp config = %s, want %s", got, want)
	}
}

// TestClaudeArgvPrependsMCPConfig is the argv-placement assertion: --mcp-config
// and its JSON lead the argv, ahead of the user's verbatim args, so retrieve is
// registered regardless of what the user passes.
func TestClaudeArgvPrependsMCPConfig(t *testing.T) {
	resp := control.Response{Port: 8080, MCPPort: 50517, Token: "tok-abc"}
	got, err := claudeArgv(resp, []string{"--resume", "--model", "opus"})
	if err != nil {
		t.Fatalf("claudeArgv: %v", err)
	}
	want := []string{
		"claude",
		"--mcp-config",
		`{"mcpServers":{"cc-squash":{"type":"http","url":"http://127.0.0.1:50517/s/tok-abc/mcp"}}}`,
		"--resume", "--model", "opus",
	}
	if !slices.Equal(got, want) {
		t.Fatalf("claudeArgv = %q, want %q", got, want)
	}
}
