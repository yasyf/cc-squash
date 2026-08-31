package control

import (
	"context"
	"errors"
	"os"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/version"
	"github.com/yasyf/daemonkit"
)

// daemonkitHomeEnv is daemonkit's home override: it resolves the state root
// through the passwd database and ignores HOME, and the constant lives in a
// package no consumer may import.
const daemonkitHomeEnv = "DAEMONKIT_HOME"

// shortHome isolates both cc-squash's state dir and daemonkit's agent root under
// a short path, because Darwin unix socket paths are capped at 104 bytes.
func shortHome(t *testing.T) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "ccs-home")
	if err != nil {
		t.Fatalf("temp home: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	t.Setenv("HOME", dir)
	t.Setenv(daemonkitHomeEnv, dir)
	if err := paths.EnsureStateDir(); err != nil {
		t.Fatalf("state dir: %v", err)
	}
	if err := os.WriteFile(paths.ConfigPath(), []byte("schema_version = 1\n"), 0o600); err != nil {
		t.Fatalf("config: %v", err)
	}
}

// controlCtx bounds a control-lane call: daemonkit refuses a Control session on
// a context with no deadline.
func controlCtx(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func TestClientPersistentBusinessRoundTrips(t *testing.T) {
	proxy := &fakeProxy{port: 50516, mcpPort: 50517}
	server := newServerWithProxy(t, proxy)
	startServer(t, server)

	client := newTestClient(t)
	awaitRegisteredPort(t, client, proxy.port)
	kill, err := client.Kill(t.Context(), true)
	if err != nil || !kill.Kill {
		t.Fatalf("kill = %+v, err = %v", kill, err)
	}
	shadow, err := client.Shadow(t.Context(), true)
	if err != nil || !shadow.Shadow {
		t.Fatalf("shadow = %+v, err = %v", shadow, err)
	}
}

func TestClientUnavailable(t *testing.T) {
	shortHome(t)
	client := newTestClient(t)
	if _, err := client.Health(controlCtx(t)); !errors.Is(err, ErrDaemonUnavailable) {
		t.Fatalf("got %v, want ErrDaemonUnavailable", err)
	}
	if _, err := client.Status(t.Context()); !errors.Is(err, ErrDaemonUnavailable) {
		t.Fatalf("status got %v, want ErrDaemonUnavailable", err)
	}
}

func TestReportedBuildAcceptsOnlyAnExactDetail(t *testing.T) {
	tests := []struct {
		name     string
		detail   []byte
		want     string
		reported bool
	}{
		{"published", (&Server{}).healthDetail(), version.String(), true},
		{"absent", nil, "", false},
		{"empty build", []byte(`{"runtime_build":""}`), "", false},
		{"foreign field", []byte(`{"runtime_build":"1.2.3","extra":1}`), "", false},
		{"not an object", []byte(`"1.2.3"`), "", false},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			build, reported := ReportedBuild(daemonkit.Health{Detail: tt.detail})
			if build != tt.want || reported != tt.reported {
				t.Fatalf("ReportedBuild = %q, %v; want %q, %v", build, reported, tt.want, tt.reported)
			}
		})
	}
}

// awaitRegisteredPort polls the status verb until the cold-started proxy's port
// has landed in the snapshot the daemon serves.
func awaitRegisteredPort(t *testing.T, client *Client, port int) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		status, err := client.Status(t.Context())
		if err == nil && status.Status != nil && status.Status.ProxyPort == port {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("proxy port %d never reached the status snapshot (last err %v)", port, err)
		}
		time.Sleep(10 * time.Millisecond)
	}
}
