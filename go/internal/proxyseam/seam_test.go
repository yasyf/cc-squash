package proxyseam

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/daemonkit/proc"
	"github.com/yasyf/daemonkit/wire"
)

// shortHome isolates the state dir under a short /tmp path: macOS caps a unix
// socket path (sun_path) at 104 bytes, and the default t.TempDir() under
// /var/folders/... overflows it once paths.ProxySocketPath() appends
// ~/.cc-squash/proxy-v1.sock.
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
	srv, err := NewServer(t.Context(), log.New(io.Discard, "", 0))
	if err != nil {
		t.Fatalf("new server: %v", err)
	}
	if err := srv.ExpectProcess(currentProcessRecord(t)); err != nil {
		t.Fatalf("expect process: %v", err)
	}
	t.Cleanup(func() { _ = srv.Close() })
	return srv
}

// dialChild connects to proxy-v1.sock as the Rust child would, after the Server has
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

func registerChild(t *testing.T, conn net.Conn, register Register) {
	t.Helper()
	frame, err := Encode(register)
	if err != nil {
		t.Fatalf("encode register: %v", err)
	}
	if _, err := conn.Write(frame); err != nil {
		t.Fatalf("write register: %v", err)
	}
}

func defaultRegister() Register {
	return Register{
		Type: MsgRegister, Protocol: ProtocolVersion, Port: 50516, MCPPort: 50517,
		Version: "0.1.0", PID: os.Getpid(),
	}
}

func currentProcessRecord(t *testing.T) proc.Identity {
	t.Helper()
	identity, err := proc.Probe(os.Getpid())
	if err != nil {
		t.Fatalf("probe current process: %v", err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatalf("current executable: %v", err)
	}
	identity.Executable, err = filepath.EvalSymlinks(executable)
	if err != nil {
		t.Fatalf("canonical current executable: %v", err)
	}
	return identity
}

func foreignProcessRecord() proc.Identity {
	return proc.Identity{
		PID: os.Getpid() + 100000, StartTime: "foreign-start", Boot: "foreign-boot", Executable: "/foreign/proxy",
	}
}

func TestServerRegisterAndMint(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	registered := make(chan Register, 1)
	go srv.Start(ctx, func(r Register) { registered <- r })

	conn := dialChild(t)
	want := defaultRegister()
	registerChild(t, conn, want)

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

func TestServerRejectsNonV1Register(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	registered := make(chan Register, 1)
	go srv.Start(ctx, func(register Register) { registered <- register })

	conn := dialChild(t)
	_, err := conn.Write([]byte(fmt.Sprintf(`{"type":"register","protocol":0,"port":50516,"mcp_port":50517,"version":"0.1.0","pid":%d}`+"\n", os.Getpid())))
	if err != nil {
		t.Fatalf("write stale register: %v", err)
	}
	if err := conn.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := conn.Read(make([]byte, 1)); err == nil {
		t.Fatal("stale proxy connection remained open")
	}
	if srv.Connected() {
		t.Fatal("stale proxy was admitted")
	}
	select {
	case got := <-registered:
		t.Fatalf("stale proxy registered: %+v", got)
	default:
	}
}

func TestServerRejectsUnpublishedProcess(t *testing.T) {
	srv := newTestServer(t)
	if err := srv.ExpectProcess(foreignProcessRecord()); err != nil {
		t.Fatalf("expect foreign process: %v", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	registered := make(chan Register, 1)
	go srv.Start(ctx, func(register Register) { registered <- register })

	conn := dialChild(t)
	frame, err := Encode(defaultRegister())
	if err != nil {
		t.Fatalf("encode register: %v", err)
	}
	// The peer-identity check may close the socket before this write races in.
	// Either outcome is valid; admission below is the invariant under test.
	_, _ = conn.Write(frame)
	if err := conn.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := conn.Read(make([]byte, 1)); err == nil {
		t.Fatal("unpublished process connection remained open")
	}
	if srv.Connected() {
		t.Fatal("unpublished process became live")
	}
	select {
	case got := <-registered:
		t.Fatalf("unpublished process registered: %+v", got)
	default:
	}
}

func TestServerRejectsRegisterPIDMismatch(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	registered := make(chan Register, 1)
	go srv.Start(ctx, func(register Register) { registered <- register })

	conn := dialChild(t)
	register := defaultRegister()
	register.PID++
	registerChild(t, conn, register)
	if err := conn.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := conn.Read(make([]byte, 1)); err == nil {
		t.Fatal("pid-mismatched connection remained open")
	}
	if srv.Connected() {
		t.Fatal("pid-mismatched process became live")
	}
	select {
	case got := <-registered:
		t.Fatalf("pid-mismatched process registered: %+v", got)
	default:
	}
}

func TestExpectProcessFencesLiveSession(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go srv.Start(ctx, func(Register) {})

	conn := dialChild(t)
	registerChild(t, conn, defaultRegister())
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := srv.ExpectProcess(foreignProcessRecord()); err != nil {
		t.Fatalf("replace expected process: %v", err)
	}
	if err := waitDisconnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := conn.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := conn.Read(make([]byte, 1)); err == nil {
		t.Fatal("superseded process connection remained open")
	}
}

func TestServerSingleEntrantRefusesLiveListener(t *testing.T) {
	_ = newTestServer(t)
	second, err := NewServer(t.Context(), log.New(io.Discard, "", 0))
	if second != nil {
		_ = second.Close()
		t.Fatal("second live listener was acquired")
	}
	if !errors.Is(err, errProxyAlreadyServing) {
		t.Fatalf("second listener error = %v, want %v", err, errProxyAlreadyServing)
	}
}

func TestCloseSettlesSilentAcceptedConnection(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan struct{})
	go func() {
		srv.Start(ctx, func(Register) {})
		close(done)
	}()

	_ = dialChild(t)
	deadline := time.Now().Add(time.Second)
	for {
		srv.mu.Lock()
		accepted := len(srv.accepted) == 1
		srv.mu.Unlock()
		if accepted {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("silent connection was not admitted")
		}
		time.Sleep(5 * time.Millisecond)
	}
	if err := srv.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("close did not settle silent admitted connection")
	}
}

func TestSendEmptyConfigDefaultsToObject(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go srv.Start(ctx, func(Register) {})

	conn := dialChild(t)
	registerChild(t, conn, defaultRegister())
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
	registerChild(t, first, defaultRegister())
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
	registerChild(t, second, defaultRegister())
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

func TestServerCancellationClosesConnectedChild(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		srv.Start(ctx, func(Register) {})
		close(done)
	}()

	conn := dialChild(t)
	registerChild(t, conn, defaultRegister())
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	cancel()

	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("Start did not return after cancellation with a connected child")
	}
	if err := waitDisconnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := conn.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("set read deadline: %v", err)
	}
	if _, err := conn.Read(make([]byte, 1)); err == nil {
		t.Fatal("connected child remained readable after cancellation")
	}
}

func TestSendShutdownDeadlineWhileWriteGateHeld(t *testing.T) {
	identity := currentProcessRecord(t)
	serverConn, childConn := net.Pipe()
	defer serverConn.Close()
	defer childConn.Close()
	srv := &Server{
		expected: identity,
		session: &session{
			conn: serverConn,
			peer: wire.Peer{
				PID: identity.PID, StartTime: identity.StartTime, Boot: identity.Boot,
				Executable: identity.Executable,
			},
			writeGate: make(chan struct{}, 1),
		},
	}
	ctx, cancel := context.WithTimeout(t.Context(), 20*time.Millisecond)
	defer cancel()
	if err := srv.SendShutdown(ctx); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("SendShutdown = %v, want deadline", err)
	}
}

func TestSendShutdownDeadlineInterruptsBlockedWrite(t *testing.T) {
	identity := currentProcessRecord(t)
	serverConn, childConn := net.Pipe()
	defer serverConn.Close()
	defer childConn.Close()
	writeGate := make(chan struct{}, 1)
	writeGate <- struct{}{}
	srv := &Server{
		expected: identity,
		session: &session{
			conn: serverConn,
			peer: wire.Peer{
				PID: identity.PID, StartTime: identity.StartTime, Boot: identity.Boot,
				Executable: identity.Executable,
			},
			writeGate: writeGate,
		},
	}
	ctx, cancel := context.WithTimeout(t.Context(), 20*time.Millisecond)
	defer cancel()
	if err := srv.SendShutdown(ctx); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("SendShutdown = %v, want deadline", err)
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
		connected := srv.session != nil
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
