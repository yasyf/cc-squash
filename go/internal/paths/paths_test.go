package paths

import (
	"path/filepath"
	"testing"
)

func TestDerivedPaths(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	for name, want := range map[string]string{
		"port":       "daemon-v1.port",
		"refs":       "refs-v1.db",
		"status":     "status-v1.json",
		"mcp config": "mcp-v1.json",
		"config":     "config.toml",
	} {
		path := map[string]string{
			"port":       PortFilePath(),
			"refs":       RefsDbPath(),
			"status":     StatusPath(),
			"mcp config": MCPConfigPath(),
			"config":     ConfigPath(),
		}[name]
		if filepath.Dir(path) != StateDir() {
			t.Fatalf("%s path = %q, outside %q", name, path, StateDir())
		}
		if filepath.Base(path) != want {
			t.Fatalf("%s path = %q, want base %q", name, path, want)
		}
	}
}
