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
	"syscall"
	"testing"
	"time"
)

// socketPair returns the two ends of one AF_UNIX stream pair. A real socketpair
// (not net.Pipe) is what a test needs here: the seam writes control frames while
// the test is elsewhere, and an unbuffered pipe would deadlock those writes into
// their timeout.
func socketPair(t *testing.T) (parent, child net.Conn) {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		t.Fatalf("socketpair: %v", err)
	}
	ends := make([]net.Conn, len(fds))
	for i, fd := range fds {
		file := os.NewFile(uintptr(fd), fmt.Sprintf("seam-%d", i))
		conn, err := net.FileConn(file)
		_ = file.Close()
		if err != nil {
			t.Fatalf("file conn: %v", err)
		}
		t.Cleanup(func() { _ = conn.Close() })
		ends[i] = conn
	}
	return ends[0], ends[1]
}

func newTestServer(t *testing.T) *Server {
	t.Helper()
	srv := NewServer(log.New(io.Discard, "", 0))
	t.Cleanup(func() { _ = srv.Close() })
	return srv
}

func serveChild(t *testing.T, ctx context.Context, srv *Server, onRegister func(Register)) (net.Conn, <-chan struct{}) {
	t.Helper()
	parent, child := socketPair(t)
	done := make(chan struct{})
	go func() {
		defer close(done)
		srv.Serve(ctx, parent, onRegister)
	}()
	return child, done
}

func defaultRegister() Register {
	return Register{
		Type: MsgRegister, Protocol: ProtocolVersion, Port: 50516, MCPPort: 50517,
		Version: "0.1.0", PID: os.Getpid(),
	}
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

func readFrame(t *testing.T, reader *bufio.Reader) any {
	t.Helper()
	line, err := reader.ReadBytes('\n')
	if err != nil {
		t.Fatalf("child read: %v", err)
	}
	message, err := Decode(line[:len(line)-1])
	if err != nil {
		t.Fatalf("child decode: %v", err)
	}
	return message
}

// waitConnected polls until the seam holds a registered child (Serve runs
// asynchronously, so a write returns before the session is established).
func waitConnected(srv *Server) error {
	return waitSession(srv, true, errors.New("proxyseam: child never registered"))
}

func waitDisconnected(srv *Server) error {
	return waitSession(srv, false, errors.New("proxyseam: child session never cleared"))
}

// waitTracked polls until Serve has entered the channel in the live set. Close
// only terminates a tracked channel, so a test that races it against track
// passes on a Close that drops nothing.
func waitTracked(srv *Server) error {
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		srv.mu.Lock()
		tracked := len(srv.live)
		srv.mu.Unlock()
		if tracked == 1 {
			return nil
		}
		time.Sleep(5 * time.Millisecond)
	}
	return errors.New("proxyseam: channel never reached the live set")
}

func waitSession(srv *Server, want bool, onTimeout error) error {
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if srv.Connected() == want {
			return nil
		}
		time.Sleep(5 * time.Millisecond)
	}
	return onTimeout
}

func TestServeRegistersAndMints(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()

	registered := make(chan Register, 1)
	child, _ := serveChild(t, ctx, srv, func(r Register) { registered <- r })

	want := defaultRegister()
	registerChild(t, child, want)
	select {
	case got := <-registered:
		if got != want {
			t.Fatalf("register = %+v, want %+v", got, want)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("onRegister never fired")
	}

	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := srv.SendMint("tok-abc", json.RawMessage(`{"k":1}`)); err != nil {
		t.Fatalf("send mint: %v", err)
	}
	mint, ok := readFrame(t, bufio.NewReader(child)).(Mint)
	if !ok {
		t.Fatal("child did not decode a mint frame")
	}
	if mint.Token != "tok-abc" || string(mint.Config) != `{"k":1}` {
		t.Fatalf("mint frame = %+v", mint)
	}
}

func TestSendBeforeRegisterFailsOpen(t *testing.T) {
	srv := newTestServer(t)
	done := make(chan error, 1)
	go func() { done <- srv.SendMint("tok-abc", nil) }()
	select {
	case err := <-done:
		if !errors.Is(err, ErrProxyNotConnected) {
			t.Fatalf("got %v, want ErrProxyNotConnected", err)
		}
	case <-time.After(time.Second):
		t.Fatal("SendMint blocked with no child registered")
	}
}

func TestServeRejectsNonV1Register(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()
	registered := make(chan Register, 1)
	child, done := serveChild(t, ctx, srv, func(r Register) { registered <- r })

	stale := fmt.Sprintf(
		`{"type":"register","protocol":0,"port":50516,"mcp_port":50517,"version":"0.1.0","pid":%d}`+"\n",
		os.Getpid(),
	)
	if _, err := child.Write([]byte(stale)); err != nil {
		t.Fatalf("write stale register: %v", err)
	}
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("stale register did not end the session")
	}
	if err := child.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := child.Read(make([]byte, 1)); !errors.Is(err, io.EOF) {
		t.Fatalf("stale child channel read = %v, want io.EOF from a closed peer", err)
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

// TestServeRejectsPostRegisterFrame pins the seam's one-way contract: the child
// speaks exactly one register frame, so anything after it ends that child's
// session before it can mutate control state.
func TestServeRejectsPostRegisterFrame(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()
	child, done := serveChild(t, ctx, srv, func(Register) {})

	registerChild(t, child, defaultRegister())
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	registerChild(t, child, defaultRegister())
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("post-register frame did not end the session")
	}
	if srv.Connected() {
		t.Fatal("child stayed registered after speaking out of turn")
	}
}

func TestCloseDropsUnregisteredChannel(t *testing.T) {
	srv := newTestServer(t)
	child, done := serveChild(t, t.Context(), srv, func(Register) {})
	if err := waitTracked(srv); err != nil {
		t.Fatal(err)
	}

	if err := srv.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("close did not settle a silent child channel")
	}
	if err := child.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := child.Read(make([]byte, 1)); !errors.Is(err, io.EOF) {
		t.Fatalf("silent child channel read = %v, want io.EOF from a closed peer", err)
	}
}

func TestSendEmptyConfigDefaultsToObject(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()
	child, _ := serveChild(t, ctx, srv, func(Register) {})

	registerChild(t, child, defaultRegister())
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := srv.SendMint("tok-abc", nil); err != nil {
		t.Fatalf("send mint: %v", err)
	}
	if got := string(readFrame(t, bufio.NewReader(child)).(Mint).Config); got != "{}" {
		t.Fatalf("empty config marshalled as %q, want {}", got)
	}
}

// TestSeamServesSuccessiveChildren is the respawn contract: one seam outlives a
// child whose channel dropped and serves the replacement the supervisor spawns.
func TestSeamServesSuccessiveChildren(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()

	first, firstDone := serveChild(t, ctx, srv, func(Register) {})
	registerChild(t, first, defaultRegister())
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	_ = first.Close()
	select {
	case <-firstDone:
	case <-time.After(2 * time.Second):
		t.Fatal("first child session did not end on channel drop")
	}
	if err := waitDisconnected(srv); err != nil {
		t.Fatal(err)
	}

	second, _ := serveChild(t, ctx, srv, func(Register) {})
	registerChild(t, second, defaultRegister())
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := srv.SendMint("tok-2", nil); err != nil {
		t.Fatalf("send to respawned child: %v", err)
	}
	if got := readFrame(t, bufio.NewReader(second)).(Mint).Token; got != "tok-2" {
		t.Fatalf("respawned child saw token %q, want tok-2", got)
	}
}

func TestServeCancellationClosesRegisteredChild(t *testing.T) {
	srv := newTestServer(t)
	ctx, cancel := context.WithCancel(t.Context())
	child, done := serveChild(t, ctx, srv, func(Register) {})

	registerChild(t, child, defaultRegister())
	if err := waitConnected(srv); err != nil {
		t.Fatal(err)
	}
	cancel()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("Serve did not return after cancellation with a registered child")
	}
	if err := waitDisconnected(srv); err != nil {
		t.Fatal(err)
	}
	if err := child.SetReadDeadline(time.Now().Add(time.Second)); err != nil {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := child.Read(make([]byte, 1)); !errors.Is(err, io.EOF) {
		t.Fatalf("registered child read = %v, want io.EOF from a closed peer", err)
	}
}

func TestSendShutdownDeadlineWhileWriteGateHeld(t *testing.T) {
	serverConn, childConn := net.Pipe()
	defer serverConn.Close()
	defer childConn.Close()
	srv := &Server{session: &session{conn: serverConn, writeGate: make(chan struct{}, 1)}}
	ctx, cancel := context.WithTimeout(t.Context(), 20*time.Millisecond)
	defer cancel()
	if err := srv.SendShutdown(ctx); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("SendShutdown = %v, want deadline", err)
	}
}

func TestSendShutdownDeadlineInterruptsBlockedWrite(t *testing.T) {
	serverConn, childConn := net.Pipe()
	defer serverConn.Close()
	defer childConn.Close()
	writeGate := make(chan struct{}, 1)
	writeGate <- struct{}{}
	srv := &Server{session: &session{conn: serverConn, writeGate: writeGate}}
	ctx, cancel := context.WithTimeout(t.Context(), 20*time.Millisecond)
	defer cancel()
	if err := srv.SendShutdown(ctx); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("SendShutdown = %v, want deadline", err)
	}
}
