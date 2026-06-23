package control

import (
	"bufio"
	"context"
	"io"
	"log"
	"net"
	"sync"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/fusekit/version"
)

// quietLogger discards daemon diagnostics so a test run stays clean.
func quietLogger(t *testing.T) *log.Logger {
	t.Helper()
	return log.New(io.Discard, "", 0)
}

// fakeProxy is a stand-in for the Rust ccs-proxy child: it dials proxy.sock once
// it exists, sends one register frame, and then drains control frames the daemon
// pushes (recording the mint tokens). It connects through the SAME
// proc.Spawn.Override seam (Server.spawnProxy) that production execs the real
// binary through, so no real proxy is ever started.
type fakeProxy struct {
	port    int
	pid     int
	version string

	mu     sync.Mutex
	mints  []string
	conn   net.Conn
	reader *bufio.Reader
}

// connect dials proxy.sock (polling until the daemon has bound it) and sends the
// register frame. It is the body the daemon's Override seam invokes.
func (f *fakeProxy) connect(t *testing.T) error {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	var conn net.Conn
	for {
		c, err := net.DialTimeout("unix", paths.ProxySocketPath(), 200*time.Millisecond)
		if err == nil {
			conn = c
			break
		}
		if time.Now().After(deadline) {
			return err
		}
		time.Sleep(10 * time.Millisecond)
	}
	frame, err := proxyseam.Encode(proxyseam.Register{
		Type: proxyseam.MsgRegister, Port: f.port, Version: f.version, PID: f.pid,
	})
	if err != nil {
		return err
	}
	if _, err := conn.Write(frame); err != nil {
		return err
	}
	f.mu.Lock()
	f.conn = conn
	f.reader = bufio.NewReader(conn)
	f.mu.Unlock()
	return nil
}

// readMint blocks until the daemon pushes one mint frame, recording and
// returning its token.
func (f *fakeProxy) readMint() (string, error) {
	f.mu.Lock()
	reader := f.reader
	f.mu.Unlock()
	line, err := reader.ReadBytes('\n')
	if err != nil {
		return "", err
	}
	msg, err := proxyseam.Decode(line[:len(line)-1])
	if err != nil {
		return "", err
	}
	tok := msg.(proxyseam.Mint).Token
	f.mu.Lock()
	f.mints = append(f.mints, tok)
	f.mu.Unlock()
	return tok, nil
}

// startServer runs srv.Run in the background under the test's isolated HOME and
// waits for its control socket to accept connections.
func startServer(t *testing.T, srv *Server) (cancel context.CancelFunc) {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		if err := srv.Run(ctx); err != nil {
			t.Errorf("Run: %v", err)
		}
	}()
	t.Cleanup(func() {
		cancel()
		<-done
	})
	if !waitSocketUp(srv.socket, 2*time.Second) {
		t.Fatal("control socket never came up")
	}
	return cancel
}

func waitSocketUp(socket string, d time.Duration) bool {
	deadline := time.Now().Add(d)
	for time.Now().Before(deadline) {
		if conn, err := net.DialTimeout("unix", socket, 200*time.Millisecond); err == nil {
			_ = conn.Close()
			return true
		}
		time.Sleep(10 * time.Millisecond)
	}
	return false
}

// newServerWithProxy builds a daemon whose Override seam launches the given fake
// proxy, under an isolated HOME.
func newServerWithProxy(t *testing.T, f *fakeProxy) *Server {
	t.Helper()
	shortHome(t)
	srv := NewServer()
	srv.log = quietLogger(t)
	if f != nil {
		// Register at the daemon's own version so the supervisor reads the proxy as
		// steady-state (co-released => same version) and never tries to replace it.
		if f.version == "" {
			f.version = version.String()
		}
		srv.spawnProxy = func() error {
			go func() {
				if err := f.connect(t); err != nil {
					t.Errorf("fake proxy connect: %v", err)
				}
			}()
			return nil
		}
	}
	return srv
}

func TestServerColdStartMint(t *testing.T) {
	f := &fakeProxy{port: 50516, pid: 4242}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	c := NewClient()
	resp, err := c.Mint()
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	if !resp.OK {
		t.Fatalf("mint not OK: %+v", resp)
	}
	if resp.Port != 50516 {
		t.Fatalf("mint port = %d, want 50516", resp.Port)
	}
	if resp.Token == "" {
		t.Fatal("mint returned an empty token")
	}
	// The fake proxy must have received the exact token over the seam.
	got, err := f.readMint()
	if err != nil {
		t.Fatalf("fake proxy read mint: %v", err)
	}
	if got != resp.Token {
		t.Fatalf("proxy saw token %q, daemon replied %q", got, resp.Token)
	}
}

func TestServerProtocolRoundTrips(t *testing.T) {
	f := &fakeProxy{port: 50600, pid: 7}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)
	c := NewClient()

	t.Run("health", func(t *testing.T) {
		resp, err := c.Health()
		if err != nil {
			t.Fatalf("health: %v", err)
		}
		if !resp.OK || resp.Version == "" {
			t.Fatalf("health resp = %+v", resp)
		}
	})

	t.Run("status", func(t *testing.T) {
		// Mint first so the snapshot reflects a session and the proxy port.
		if _, err := c.Mint(); err != nil {
			t.Fatalf("mint: %v", err)
		}
		if _, err := f.readMint(); err != nil {
			t.Fatalf("drain mint: %v", err)
		}
		resp, err := c.Status()
		if err != nil {
			t.Fatalf("status: %v", err)
		}
		if resp.Status == nil {
			t.Fatal("status snapshot missing")
		}
		if resp.Status.ProxyPort != 50600 || resp.Status.ProxyPID != 7 {
			t.Fatalf("status proxy = port %d pid %d", resp.Status.ProxyPort, resp.Status.ProxyPID)
		}
		if resp.Status.Sessions != 1 {
			t.Fatalf("status sessions = %d, want 1", resp.Status.Sessions)
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

	t.Run("unknown-op", func(t *testing.T) {
		resp, err := (&Client{socket: srv.socket}).do(Request{Op: "bogus"}, 2*time.Second)
		if err != nil {
			t.Fatalf("do: %v", err)
		}
		if resp.OK {
			t.Fatalf("unknown op replied OK: %+v", resp)
		}
	})
}

func TestServerMintDemux(t *testing.T) {
	f := &fakeProxy{port: 50700, pid: 9}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	const n = 8
	tokens := make(chan string, n)
	var wg sync.WaitGroup
	for range n {
		wg.Add(1)
		go func() {
			defer wg.Done()
			resp, err := NewClient().Mint()
			if err != nil {
				t.Errorf("mint: %v", err)
				return
			}
			tokens <- resp.Token
		}()
	}
	wg.Wait()
	close(tokens)

	seen := map[string]bool{}
	for tok := range tokens {
		if tok == "" {
			t.Fatal("empty token")
		}
		if seen[tok] {
			t.Fatalf("duplicate token %q", tok)
		}
		seen[tok] = true
	}
	if len(seen) != n {
		t.Fatalf("got %d unique tokens, want %d", len(seen), n)
	}
}

func TestServerSeamFailOpen(t *testing.T) {
	// A daemon whose proxy NEVER registers: spawnProxy returns nil without
	// connecting, so the seam stays empty.
	shortHome(t)
	srv := NewServer()
	srv.log = quietLogger(t)
	srv.spawnProxy = func() error { return nil }
	srv.mintTimeout = 200 * time.Millisecond
	startServer(t, srv)

	// OpMint must not hang past the ready timeout, and with no proxy port known
	// it must reply with a graceful error rather than wedging the daemon.
	start := time.Now()
	resp, err := NewClient().Mint()
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("mint took %s; the ready wait did not bound it", elapsed)
	}
	if resp.OK {
		t.Fatalf("mint reported OK with no proxy port: %+v", resp)
	}
	if resp.Error == "" {
		t.Fatal("mint failed open but gave no error message")
	}

	// The daemon is still alive: Health answers immediately.
	if h, err := NewClient().Health(); err != nil || !h.OK {
		t.Fatalf("daemon wedged after a fail-open mint: resp=%+v err=%v", h, err)
	}
}

func TestServerSingleEntrant(t *testing.T) {
	f := &fakeProxy{port: 50800, pid: 11}
	first := newServerWithProxy(t, f)
	startServer(t, first)

	// A second daemon under the SAME HOME races the bound socket. Its evict
	// closure probes the live first daemon, sees the same version, and refuses to
	// bind.
	second := NewServer()
	second.log = quietLogger(t)
	second.spawnProxy = func() error { return nil }
	err := second.Run(context.Background())
	if err == nil {
		t.Fatal("second daemon bound the socket; single-entrant guard failed")
	}
}

func TestServerStatusFileWritten(t *testing.T) {
	f := &fakeProxy{port: 50900, pid: 13}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	// onRegister writes status.json atomically once the proxy registers; poll for
	// it (the bring-up is asynchronous).
	deadline := time.Now().Add(3 * time.Second)
	for {
		snap, err := ReadStatus()
		if err == nil && snap.ProxyPort == 50900 {
			if snap.ProxyPID != 13 {
				t.Fatalf("status.json pid = %d, want 13", snap.ProxyPID)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("status.json never reflected the proxy port (last err %v)", err)
		}
		time.Sleep(10 * time.Millisecond)
	}

	// The published port-file matches.
	if port, err := ReadPort(); err != nil || port != 50900 {
		t.Fatalf("port-file = %d (err %v), want 50900", port, err)
	}
}
