package cli

import (
	"slices"
	"testing"
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
