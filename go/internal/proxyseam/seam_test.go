package proxyseam

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"log"
	"net"
	"os"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// shortHome isolates the state dir under a short /tmp path: macOS caps a unix
// socket path (sun_path) at 104 bytes, and the default t.TempDir() under
// /var/folders/... overflows it once paths.ProxySocketPath() appends
// ~/.cc-squash/proxy.sock.
func shortHome(t *testing.T) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "ccs-home")
	if err != nil {
		t.Fatalf("temp home: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	t.Setenv("HOME", dir)
}

func newTestServer(t *testing.T) *Server {
	t.Helper()
	shortHome(t)
	srv, err := NewServer(log.New(io.Discard, "", 0))
	if err != nil {
		t.Fatalf("new server: %v", err)
	}
	t.Cleanup(func() { _ = srv.Close() })
	return srv
}

// dialChild connects to proxy.sock as the Rust child would, after the Server has
// bound it.
func dialChild(t *testing.T) net.Conn {
	t.Helper()
	conn, err := net.DialTimeout("unix", paths.ProxySocketPath(), time.Second)
	if err != nil {
		t.Fatalf("child dial: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	return conn
}

func TestServerRegisterAndMint(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	registered := make(chan Register, 1)
	go srv.Start(ctx, func(r Register) { registered <- r })

	conn := dialChild(t)
	want := Register{Type: MsgRegister, Port: 50516, Version: "0.1.0", PID: 4242}
	frame, err := Encode(want)
	if err != nil {
		t.Fatalf("encode register: %v", err)
	}
	if _, err := conn.Write(frame); err != nil {
		t.Fatalf("write register: %v", err)
	}

	select {
	case got := <-registered:
		if got != want {
			t.Fatalf("register mismatch: got %+v, want %+v", got, want)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("onRegister never fired")
	}

	// The control plane mints a token; the fake child must decode the exact frame.
	reader := bufio.NewReader(conn)
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := srv.SendMint("tok-abc", json.RawMessage(`{"k":1}`)); err != nil {
		t.Fatalf("send mint: %v", err)
	}
	line, err := reader.ReadBytes('\n')
	if err != nil {
		t.Fatalf("child read mint: %v", err)
	}
	decoded, err := Decode(line[:len(line)-1])
	if err != nil {
		t.Fatalf("child decode mint: %v", err)
	}
	mint, ok := decoded.(Mint)
	if !ok {
		t.Fatalf("decoded %T, want Mint", decoded)
	}
	if mint.Token != "tok-abc" || string(mint.Config) != `{"k":1}` {
		t.Fatalf("mint frame = %+v", mint)
	}
}

func TestSendBeforeConnectFailsOpen(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go srv.Start(ctx, func(Register) {})

	// No child has connected: the send returns the fail-open sentinel and does
	// not block (a 1s budget is generous for an in-memory mutex check).
	done := make(chan error, 1)
	go func() { done <- srv.SendMint("tok-abc", nil) }()
	select {
	case err := <-done:
		if err != ErrProxyNotConnected {
			t.Fatalf("got %v, want ErrProxyNotConnected", err)
		}
	case <-time.After(time.Second):
		t.Fatal("SendMint blocked with no child connected")
	}
}

func TestSendEmptyConfigDefaultsToObject(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go srv.Start(ctx, func(Register) {})

	conn := dialChild(t)
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := srv.SendMint("tok-abc", nil); err != nil {
		t.Fatalf("send mint: %v", err)
	}
	line, err := bufio.NewReader(conn).ReadBytes('\n')
	if err != nil {
		t.Fatalf("child read: %v", err)
	}
	mint, _ := Decode(line[:len(line)-1])
	if got := string(mint.(Mint).Config); got != "{}" {
		t.Fatalf("empty config marshalled as %q, want {}", got)
	}
}

func TestServerSurvivesChildReconnect(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go srv.Start(ctx, func(Register) {})

	// First child connects, then drops.
	first, err := net.DialTimeout("unix", paths.ProxySocketPath(), time.Second)
	if err != nil {
		t.Fatalf("first dial: %v", err)
	}
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	_ = first.Close()
	// The accept loop serves one child at a time, so the server stays inside
	// serveConn(first) until it observes the close. Wait for that teardown before
	// dialing the successor, so SendMint below can never target the stale conn.
	if err := waitDisconnected(srv); err != nil {
		t.Fatal(err)
	}

	// A respawned child reconnects to the still-open listener and can be minted.
	second := dialChild(t)
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := srv.SendMint("tok-2", nil); err != nil {
		t.Fatalf("send to reconnected child: %v", err)
	}
	if _, err := bufio.NewReader(second).ReadBytes('\n'); err != nil {
		t.Fatalf("reconnected child read: %v", err)
	}
}

// waitConnected polls until the server has a live child connection (Start runs
// asynchronously, so a dial returns before serveConn registers the conn) or a
// short deadline elapses.
func waitConnected(srv *Server) error {
	return waitConn(srv, true, errProbe("proxyseam: child never connected"))
}

// waitDisconnected polls until the server has cleared its child connection.
func waitDisconnected(srv *Server) error {
	return waitConn(srv, false, errProbe("proxyseam: child connection never cleared"))
}

func waitConn(srv *Server, want bool, onTimeout error) error {
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		srv.mu.Lock()
		connected := srv.conn != nil
		srv.mu.Unlock()
		if connected == want {
			return nil
		}
		time.Sleep(5 * time.Millisecond)
	}
	return onTimeout
}

type errProbe string

func (e errProbe) Error() string { return string(e) }
