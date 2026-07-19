package control

import (
	"context"
	"errors"
	"os"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/version"
)

// shortHome isolates the state dir under a short path because Darwin unix
// socket paths are capped at 104 bytes.
func shortHome(t *testing.T) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "ccs-home")
	if err != nil {
		t.Fatalf("temp home: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	t.Setenv("HOME", dir)
}

func TestClientPersistentBusinessAndLifecycleRoundTrips(t *testing.T) {
	proxy := &fakeProxy{port: 50516, mcpPort: 50517, pid: 4242}
	server := newServerWithProxy(t, proxy)
	startServer(t, server)

	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	health, err := client.Health(t.Context())
	if err != nil {
		t.Fatalf("health: %v", err)
	}
	if health.Build != version.String() {
		t.Fatalf("health build = %q, want %q", health.Build, version.String())
	}
	status, err := client.Status(t.Context())
	if err != nil || status.Status == nil || status.Status.ProxyPort != 50516 {
		t.Fatalf("status = %+v, err = %v", status, err)
	}
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
	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	if _, err := client.Health(t.Context()); !errors.Is(err, ErrDaemonUnavailable) {
		t.Fatalf("got %v, want ErrDaemonUnavailable", err)
	}
}

func TestEnsureCurrentShortCircuitsAtExactRelease(t *testing.T) {
	server := newServerWithProxy(t, &fakeProxy{port: 50516, pid: 4242})
	startServer(t, server)
	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	if err := client.EnsureCurrent(t.Context(), time.Second); err != nil {
		t.Fatalf("EnsureCurrent: %v", err)
	}
}

func TestWaitGoneObservesRuntimeShutdown(t *testing.T) {
	proxy := &fakeProxy{port: 50516, pid: 4242}
	server := newServerWithProxy(t, proxy)
	startServer(t, server)
	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	deadline := time.Now().Add(3 * time.Second)
	for {
		status, err := client.Status(t.Context())
		if err == nil && status.Status != nil && status.Status.ProxyPort == proxy.port {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("proxy never registered before shutdown")
		}
		time.Sleep(10 * time.Millisecond)
	}
	seen := proxy.stepDownOnShutdown(t)
	if err := client.Shutdown(t.Context()); err != nil {
		t.Fatalf("shutdown: %v", err)
	}
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	defer cancel()
	if err := client.WaitGone(ctx, 4*time.Second); err != nil {
		t.Fatalf("WaitGone: %v", err)
	}
	select {
	case <-seen:
	case <-ctx.Done():
		t.Fatal("proxy did not observe orderly shutdown")
	}
}
