package config

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// homeWithConfig isolates HOME under /tmp and, when toml is non-empty, writes it
// to ~/.cc-squash/config.toml so Load reads it. An empty toml leaves the file
// absent — the engine-defaults case.
func homeWithConfig(t *testing.T, toml string) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "ccs-cfg")
	if err != nil {
		t.Fatalf("temp home: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	t.Setenv("HOME", dir)
	if toml == "" {
		return
	}
	if err := os.MkdirAll(filepath.Dir(paths.ConfigPath()), 0o700); err != nil {
		t.Fatalf("mkdir state dir: %v", err)
	}
	if err := os.WriteFile(paths.ConfigPath(), []byte(toml), 0o600); err != nil {
		t.Fatalf("write config.toml: %v", err)
	}
}

func TestLoad(t *testing.T) {
	cases := []struct {
		name string
		toml string
		want string
	}{
		{
			name: "absent file yields empty object",
			toml: "",
			want: `{}`,
		},
		{
			name: "econ and policy keys round-trip to snake_case json",
			toml: `[economics]
npv_floor = 0.5
ttl_forced_s = 120.0

[policy]
recency_window_n = 5
cache_hint_cap = 8
`,
			want: `{"economics":{"npv_floor":0.5,"ttl_forced_s":120},"policy":{"recency_window_n":5,"cache_hint_cap":8}}`,
		},
		{
			name: "unset section is omitted entirely",
			toml: `[policy]
lookback_positions = 40
`,
			want: `{"policy":{"lookback_positions":40}}`,
		},
		{
			name: "empty config file yields empty object",
			toml: "# just a comment\n",
			want: `{}`,
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			homeWithConfig(t, c.toml)
			got, err := Load()
			if err != nil {
				t.Fatalf("Load: %v", err)
			}
			if !jsonEqual(t, got, c.want) {
				t.Fatalf("Load() = %s, want %s", got, c.want)
			}
		})
	}
}

// jsonEqual compares two JSON documents for semantic (key-order-independent)
// equality, so the test pins the emitted keys without depending on Go's map
// iteration order.
func jsonEqual(t *testing.T, got json.RawMessage, want string) bool {
	t.Helper()
	var a, b any
	if err := json.Unmarshal(got, &a); err != nil {
		t.Fatalf("unmarshal got %s: %v", got, err)
	}
	if err := json.Unmarshal([]byte(want), &b); err != nil {
		t.Fatalf("unmarshal want %s: %v", want, err)
	}
	ja, _ := json.Marshal(a)
	jb, _ := json.Marshal(b)
	return string(ja) == string(jb)
}
