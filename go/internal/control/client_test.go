package control

import (
	"encoding/json"
	"net"
	"os"
	"sync"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// shortHome isolates the state dir like t.Setenv("HOME", t.TempDir()) would, but
// roots it under a short /tmp path: macOS caps a unix socket path (sun_path) at
// 104 bytes, and the default t.TempDir() under /var/folders/... overflows it
// once paths.SocketPath() appends ~/.cc-squash/daemon.sock.
func shortHome(t *testing.T) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "ccs-home")
	if err != nil {
		t.Fatalf("temp home: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	t.Setenv("HOME", dir)
}

// fakeDaemon binds the control socket under the test's isolated HOME and replies
// to each request with reply(req). It mirrors the daemon's flat newline-delimited
// JSON: decode one Request, encode one Response. It is the mock boundary the
// client talks to; the client under test stays real.
func fakeDaemon(t *testing.T, reply func(Request) Response) {
	t.Helper()
	if err := paths.EnsureStateDir(); err != nil {
		t.Fatalf("ensure state dir: %v", err)
	}
	ln, err := net.Listen("unix", paths.SocketPath())
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	var wg sync.WaitGroup
	t.Cleanup(func() {
		_ = ln.Close()
		wg.Wait()
	})
	wg.Add(1)
	go func() {
		defer wg.Done()
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			wg.Add(1)
			go func() {
				defer wg.Done()
				defer func() { _ = conn.Close() }()
				var req Request
				if err := json.NewDecoder(conn).Decode(&req); err != nil {
					return
				}
				resp := reply(req)
				resp.Proto = ProtocolVersion
				_ = json.NewEncoder(conn).Encode(resp)
			}()
		}
	}()
}

func TestClientRoundTrip(t *testing.T) {
	shortHome(t)
	snap := &StatusSnapshot{Proto: ProtocolVersion, Version: "9.9.9", ProxyPort: 50516, Sessions: 2}
	fakeDaemon(t, func(req Request) Response {
		if req.Proto != ProtocolVersion {
			t.Errorf("server saw proto %d, want %d", req.Proto, ProtocolVersion)
		}
		switch req.Op {
		case OpHealth:
			return Response{OK: true, Version: "9.9.9"}
		case OpStatus:
			return Response{OK: true, Version: "9.9.9", Status: snap}
		case OpMint:
			return Response{OK: true, Token: "tok-xyz"}
		case OpKill:
			return Response{OK: true, Kill: req.On}
		case OpShadow:
			return Response{OK: true, Shadow: req.On}
		case OpShutdown:
			return Response{OK: true}
		default:
			return Response{OK: false, Error: "unknown op: " + string(req.Op)}
		}
	})

	c := NewClient()
	if !c.Available() {
		t.Fatal("Available reported the fake daemon down")
	}

	t.Run("health", func(t *testing.T) {
		resp, err := c.Health()
		if err != nil {
			t.Fatalf("health: %v", err)
		}
		if !resp.OK || resp.Version != "9.9.9" {
			t.Fatalf("health resp = %+v", resp)
		}
	})

	t.Run("status", func(t *testing.T) {
		resp, err := c.Status()
		if err != nil {
			t.Fatalf("status: %v", err)
		}
		if resp.Status == nil || *resp.Status != *snap {
			t.Fatalf("status resp = %+v", resp)
		}
	})

	t.Run("mint", func(t *testing.T) {
		resp, err := c.Mint()
		if err != nil {
			t.Fatalf("mint: %v", err)
		}
		if resp.Token != "tok-xyz" {
			t.Fatalf("mint token = %q", resp.Token)
		}
	})

	t.Run("kill", func(t *testing.T) {
		resp, err := c.Kill(true)
		if err != nil {
			t.Fatalf("kill: %v", err)
		}
		if !resp.Kill {
			t.Fatalf("kill resp = %+v", resp)
		}
	})

	t.Run("shadow", func(t *testing.T) {
		resp, err := c.Shadow(true)
		if err != nil {
			t.Fatalf("shadow: %v", err)
		}
		if !resp.Shadow {
			t.Fatalf("shadow resp = %+v", resp)
		}
	})

	t.Run("shutdown", func(t *testing.T) {
		resp, err := c.Shutdown()
		if err != nil {
			t.Fatalf("shutdown: %v", err)
		}
		if !resp.OK {
			t.Fatalf("shutdown resp = %+v", resp)
		}
	})
}

func TestClientDoUnavailable(t *testing.T) {
	shortHome(t)
	if _, err := NewClient().Health(); err != ErrDaemonUnavailable {
		t.Fatalf("got %v, want ErrDaemonUnavailable", err)
	}
}

func TestEnsureRunningShortCircuitsWhenAvailable(t *testing.T) {
	shortHome(t)
	fakeDaemon(t, func(Request) Response { return Response{OK: true} })
	// Available() is true, so EnsureRunning must report up without ever spawning
	// a child (no `ccs daemon` binary exists in the test process).
	if !NewClient().EnsureRunning(time.Second) {
		t.Fatal("EnsureRunning reported down while the socket was up")
	}
}

func TestWaitGoneDetectsClose(t *testing.T) {
	shortHome(t)
	if err := paths.EnsureStateDir(); err != nil {
		t.Fatalf("ensure state dir: %v", err)
	}
	ln, err := net.Listen("unix", paths.SocketPath())
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	c := NewClient()
	if c.WaitGone(100 * time.Millisecond) {
		t.Fatal("WaitGone reported dead while the socket was up")
	}
	_ = ln.Close()
	if !c.WaitGone(time.Second) {
		t.Fatal("WaitGone did not detect the closed socket")
	}
}
