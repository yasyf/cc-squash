package paths

import (
	"path/filepath"
	"testing"
)

func TestDerivedPaths(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	for name, path := range map[string]string{
		"port":              PortFilePath(),
		"proxy seam":        ProxySocketPath(),
		"refs":              RefsDbPath(),
		"status":            StatusPath(),
		"mcp config":        MCPConfigPath(),
		"worker processes":  WorkerProcessStorePath(),
		"child processes":   ChildProcessStorePath(),
		"services":          ServiceStatePath(),
		"service processes": ServiceProcessStorePath(),
	} {
		if filepath.Dir(path) != StateDir() {
			t.Fatalf("%s path = %q, outside %q", name, path, StateDir())
		}
	}
	if filepath.Base(PortFilePath()) != "daemon-v1.port" ||
		filepath.Base(ProxySocketPath()) != "proxy-v1.sock" ||
		filepath.Base(RefsDbPath()) != "refs-v1.db" ||
		filepath.Base(StatusPath()) != "status-v1.json" ||
		filepath.Base(MCPConfigPath()) != "mcp-v1.json" ||
		filepath.Base(WorkerProcessStorePath()) != "worker-processes-v1.db" ||
		filepath.Base(ChildProcessStorePath()) != "child-processes-v1.db" ||
		filepath.Base(ServiceStatePath()) != "services-v1.db" ||
		filepath.Base(ServiceProcessStorePath()) != "service-processes-v1.db" {
		t.Fatal("derived paths are not the exact epoch names")
	}
}
