package control

import (
	"context"
	"errors"
	"os"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/version"
	"github.com/yasyf/daemonkit/wire"
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
	if err := paths.EnsureStateDir(); err != nil {
		t.Fatalf("state dir: %v", err)
	}
	if err := os.WriteFile(paths.ConfigPath(), []byte("schema_version = 1\n"), 0o600); err != nil {
		t.Fatalf("config: %v", err)
	}
}

func TestClientPersistentBusinessRoundTrips(t *testing.T) {
	proxy := &fakeProxy{port: 50516, mcpPort: 50517, pid: 4242}
	server := newServerWithProxy(t, proxy)
	startServer(t, server)

	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	health, err := client.RuntimeHealth(t.Context())
	if err != nil {
		t.Fatalf("health: %v", err)
	}
	if health.RuntimeBuild != version.String() {
		t.Fatalf("health build = %q, want %q", health.RuntimeBuild, version.String())
	}
	deadline := time.Now().Add(3 * time.Second)
	var status Response
	for {
		status, err = client.Status(t.Context())
		if err == nil && status.Status != nil && status.Status.ProxyPort == 50516 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("status = %+v, err = %v", status, err)
		}
		time.Sleep(10 * time.Millisecond)
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
	if _, err := client.RuntimeHealth(t.Context()); !errors.Is(err, ErrDaemonUnavailable) {
		t.Fatalf("got %v, want ErrDaemonUnavailable", err)
	}
}

func TestRuntimeHealthRequiresExactIdentityAndState(t *testing.T) {
	health := RuntimeHealth{
		RuntimeBuild: version.String(), RuntimeProtocol: int(wire.ProtocolVersion), PID: 42,
		ProcessGeneration: "process-generation", Ready: true, State: RuntimeStateHealthy,
	}
	if err := validateRuntimeHealth(health); err != nil {
		t.Fatalf("valid health: %v", err)
	}
	client := newClient("/tmp/unused", WireBuild, version.String())
	if !client.current(health) {
		t.Fatal("exact healthy runtime was not current")
	}

	tests := map[string]func(*RuntimeHealth){
		"build":    func(h *RuntimeHealth) { h.RuntimeBuild = "" },
		"protocol": func(h *RuntimeHealth) { h.RuntimeProtocol = 0 },
		"pid":      func(h *RuntimeHealth) { h.PID = 1 },
		"generation": func(h *RuntimeHealth) {
			h.ProcessGeneration = ""
		},
		"state": func(h *RuntimeHealth) { h.State = "unknown" },
	}
	for name, mutate := range tests {
		t.Run(name, func(t *testing.T) {
			invalid := health
			mutate(&invalid)
			if err := validateRuntimeHealth(invalid); err == nil {
				t.Fatalf("validateRuntimeHealth(%+v) succeeded", invalid)
			}
		})
	}
	for name, mutate := range map[string]func(*RuntimeHealth){
		"wrong build": func(h *RuntimeHealth) { h.RuntimeBuild = "other" },
		"not ready":   func(h *RuntimeHealth) { h.Ready = false },
		"draining":    func(h *RuntimeHealth) { h.Draining = true },
		"degraded":    func(h *RuntimeHealth) { h.State = RuntimeStateDegraded },
	} {
		t.Run(name, func(t *testing.T) {
			notCurrent := health
			mutate(&notCurrent)
			if client.current(notCurrent) {
				t.Fatalf("runtime unexpectedly current: %+v", notCurrent)
			}
		})
	}
}

func TestWaitReadyObservesExactRelease(t *testing.T) {
	server := newServerWithProxy(t, &fakeProxy{port: 50516, pid: 4242})
	startServer(t, server)
	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	if err := client.WaitReady(t.Context(), time.Second); err != nil {
		t.Fatalf("WaitReady: %v", err)
	}
}

func TestWaitGoneObservesRuntimeShutdown(t *testing.T) {
	proxy := &fakeProxy{port: 50516, pid: 4242}
	server := newServerWithProxy(t, proxy)
	cancelServer := startServer(t, server)
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
	cancelServer()
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
