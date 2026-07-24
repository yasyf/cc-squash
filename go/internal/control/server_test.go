package control

import (
	"bufio"
	"context"
	"io"
	"log"
	"net"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/cc-squash/go/internal/version"
	"github.com/yasyf/daemonkit/daemonrole"
	"github.com/yasyf/daemonkit/proc"
	"github.com/yasyf/daemonkit/wire"
)

// quietLogger discards daemon diagnostics so a test run stays clean.
func quietLogger(t *testing.T) *log.Logger {
	t.Helper()
	return log.New(io.Discard, "", 0)
}

func testDaemonRole(t *testing.T) daemonrole.Classifier {
	t.Helper()
	executable, err := os.Executable()
	if err != nil {
		t.Fatalf("resolve test executable: %v", err)
	}
	return daemonrole.Classifier{RoleID: DaemonRoleID, RolePath: filepath.Clean(executable)}
}

func testProcessRecord(t *testing.T) proc.Record {
	t.Helper()
	identity, err := proc.Probe(os.Getpid())
	if err != nil {
		t.Fatalf("probe test process: %v", err)
	}
	return proc.Record{
		RecoveryClass: proc.RecoveryTask, PID: identity.PID, StartTime: identity.StartTime,
		Comm: identity.Comm, Boot: identity.Boot, Generation: "test-generation",
	}
}

// fakeProxy is a stand-in for the Rust ccs-proxy child: it dials proxy-v1.sock once
// it exists, sends one register frame, and then drains control frames the daemon
// pushes (recording the mint tokens). It connects through the server's explicit
// test launch seam, so no real proxy is ever started.
type fakeProxy struct {
	port    int
	mcpPort int
	pid     int
	version string

	mu     sync.Mutex
	mints  []string
	conn   net.Conn
	reader *bufio.Reader
}

// connect dials proxy-v1.sock (polling until the daemon has bound it) and sends the
// register frame. It is the body the daemon's test launch seam invokes.
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
	mcpPort := f.mcpPort
	if mcpPort == 0 {
		mcpPort = f.port + 1
	}
	frame, err := proxyseam.Encode(proxyseam.Register{
		Type: proxyseam.MsgRegister, Protocol: proxyseam.ProtocolVersion,
		Port: f.port, MCPPort: mcpPort, Version: f.version, PID: f.pid,
	})
	if err != nil {
		return err
	}
	f.mu.Lock()
	f.conn = conn
	f.reader = bufio.NewReader(conn)
	f.mu.Unlock()
	if _, err := conn.Write(frame); err != nil {
		f.mu.Lock()
		f.conn = nil
		f.reader = nil
		f.mu.Unlock()
		_ = conn.Close()
		return err
	}
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

// readMintFrame blocks until the daemon pushes one mint frame and returns it
// whole, so a test can assert the per-session config rode along with the token.
func (f *fakeProxy) readMintFrame() (proxyseam.Mint, error) {
	f.mu.Lock()
	reader := f.reader
	f.mu.Unlock()
	line, err := reader.ReadBytes('\n')
	if err != nil {
		return proxyseam.Mint{}, err
	}
	msg, err := proxyseam.Decode(line[:len(line)-1])
	if err != nil {
		return proxyseam.Mint{}, err
	}
	return msg.(proxyseam.Mint), nil
}

// readFrame blocks until the daemon pushes one control frame and returns it
// decoded — the generic counterpart of readMint, used to observe the shutdown
// frame the teardown sends.
func (f *fakeProxy) readFrame() (any, error) {
	f.mu.Lock()
	reader := f.reader
	f.mu.Unlock()
	line, err := reader.ReadBytes('\n')
	if err != nil {
		return nil, err
	}
	return proxyseam.Decode(line[:len(line)-1])
}

// stepDownOnShutdown drains frames until a Shutdown arrives, then closes the
// seam connection — modelling the real proxy stepping down so the daemon's
// WaitGone observes the drop. It reports the observed shutdown on a channel.
func (f *fakeProxy) stepDownOnShutdown(t *testing.T) <-chan struct{} {
	t.Helper()
	seen := make(chan struct{})
	go func() {
		for {
			msg, err := f.readFrame()
			if err != nil {
				return
			}
			if _, ok := msg.(proxyseam.Shutdown); ok {
				f.mu.Lock()
				conn := f.conn
				f.mu.Unlock()
				_ = conn.Close()
				close(seen)
				return
			}
		}
	}()
	return seen
}

// startServer runs srv.Run and waits for exact runtime admission.
func startServer(t *testing.T, srv *Server) (cancel context.CancelFunc) {
	t.Helper()
	cancel = startServerSocket(t, srv)
	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	if err := client.WaitReady(t.Context(), 2*time.Second); err != nil {
		t.Fatalf("runtime never became ready: %v", err)
	}
	return cancel
}

// startServerSocket waits only for the pre-admission health socket.
func startServerSocket(t *testing.T, srv *Server) (cancel context.CancelFunc) {
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
	srv, err := NewServer(testDaemonRole(t))
	if err != nil {
		t.Fatalf("NewServer: %v", err)
	}
	srv.log = quietLogger(t)
	if f != nil {
		f.pid = os.Getpid()
		// Register at the daemon's own version so the supervisor reads the proxy as
		// steady-state (co-released => same version) and never tries to replace it.
		if f.version == "" {
			f.version = version.String()
		}
		srv.spawnProxy = func(recorded func(proc.Record) error) error {
			if err := recorded(testProcessRecord(t)); err != nil {
				return err
			}
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
	t.Cleanup(func() { _ = c.Close() })
	resp, err := c.Mint(t.Context())
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
	status, err := ReadStatus()
	if err != nil {
		t.Fatalf("read status after mint: %v", err)
	}
	if status.ProxyPort != resp.Port || status.ProxyPID != f.pid {
		t.Fatalf("status after mint = %+v, want proxy port %d pid %d", status, resp.Port, f.pid)
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

// TestServerMintReturnsMCPPort is the 4e contract assertion: the proxy's
// register frame carries the SECOND listener's mcp_port, the daemon records it,
// and the mint reply surfaces it so `ccs run` can build the retrieve --mcp-config
// URL off one round-trip.
func TestServerMintReturnsMCPPort(t *testing.T) {
	f := &fakeProxy{port: 50516, mcpPort: 50517, pid: 4242}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	resp, err := client.Mint(t.Context())
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	if resp.MCPPort != 50517 {
		t.Fatalf("mint mcp_port = %d, want 50517", resp.MCPPort)
	}
	if _, err := f.readMint(); err != nil {
		t.Fatalf("drain mint: %v", err)
	}

	// The status snapshot mirrors the recorded mcp_port too.
	st, err := client.Status(t.Context())
	if err != nil {
		t.Fatalf("status: %v", err)
	}
	if st.Status.ProxyMCPort != 50517 {
		t.Fatalf("status proxy_mcp_port = %d, want 50517", st.Status.ProxyMCPort)
	}
}

// TestServerGcForwardsFrame is the `ccs gc` dispatch assertion: an OpGc control
// request forwards a single {"type":"gc"} seam frame to the proxy, which runs
// store.gc against the reachable set.
func TestServerGcForwardsFrame(t *testing.T) {
	f := &fakeProxy{port: 50520, mcpPort: 50521, pid: 99}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	// Wait for the cold-start register so the seam has a live child to push to.
	deadline := time.Now().Add(3 * time.Second)
	for {
		if snap, err := ReadStatus(); err == nil && snap.ProxyPort == 50520 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("proxy never registered before gc")
		}
		time.Sleep(10 * time.Millisecond)
	}

	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	resp, err := client.Gc(t.Context())
	if err != nil {
		t.Fatalf("gc: %v", err)
	}
	if !resp.OK {
		t.Fatalf("gc not OK: %+v", resp)
	}
	frame, err := f.readFrame()
	if err != nil {
		t.Fatalf("read gc frame: %v", err)
	}
	if _, ok := frame.(proxyseam.Gc); !ok {
		t.Fatalf("proxy saw %T, want proxyseam.Gc", frame)
	}
}

// TestServerMintCarriesConfig is the 4a seam assertion: handleMint must push the
// config loaded from config.toml (not nil) over the seam, so the proxy mints each
// session with the user's relay knobs. A daemon with a config.toml on disk pushes
// exactly that JSON on the mint frame.
func TestServerMintCarriesConfig(t *testing.T) {
	f := &fakeProxy{port: 51200, pid: 23}
	srv := newServerWithProxy(t, f) // sets the isolated HOME via shortHome
	writeTestConfig(t, "[economics]\nnpv_floor = 0.25\n")
	startServer(t, srv)

	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	if _, err := client.Mint(t.Context()); err != nil {
		t.Fatalf("mint: %v", err)
	}
	mint, err := f.readMintFrame()
	if err != nil {
		t.Fatalf("read mint frame: %v", err)
	}
	if string(mint.Config) != `{"economics":{"npv_floor":0.25}}` {
		t.Fatalf("mint config = %s, want the loaded config.toml (not nil/{})", mint.Config)
	}
}

// writeTestConfig writes config.toml under the already-isolated test HOME so the
// daemon's startup config.Load reads it.
func writeTestConfig(t *testing.T, toml string) {
	t.Helper()
	if err := os.MkdirAll(paths.StateDir(), 0o700); err != nil {
		t.Fatalf("mkdir state dir: %v", err)
	}
	if err := os.WriteFile(paths.ConfigPath(), []byte("schema_version = 1\n"+toml), 0o600); err != nil {
		t.Fatalf("write config.toml: %v", err)
	}
}

func TestServerProtocolRoundTrips(t *testing.T) {
	f := &fakeProxy{port: 50600, pid: 7}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)
	c := NewClient()
	t.Cleanup(func() { _ = c.Close() })

	t.Run("health", func(t *testing.T) {
		health, err := c.RuntimeHealth(t.Context())
		if err != nil {
			t.Fatalf("health: %v", err)
		}
		if health.RuntimeBuild != version.String() || health.RuntimeProtocol != int(wire.ProtocolVersion) ||
			health.PID <= 1 || health.ProcessGeneration == "" || !health.Ready ||
			health.State != "healthy" || health.Draining || health.Busy {
			t.Fatalf("health = %+v", health)
		}
	})

	t.Run("status", func(t *testing.T) {
		// Mint first so the snapshot reflects a session and the proxy port.
		if _, err := c.Mint(t.Context()); err != nil {
			t.Fatalf("mint: %v", err)
		}
		if _, err := f.readMint(); err != nil {
			t.Fatalf("drain mint: %v", err)
		}
		resp, err := c.Status(t.Context())
		if err != nil {
			t.Fatalf("status: %v", err)
		}
		if resp.Status == nil {
			t.Fatal("status snapshot missing")
		}
		if resp.Status.ProxyPort != 50600 || resp.Status.ProxyPID != f.pid {
			t.Fatalf("status proxy = port %d pid %d", resp.Status.ProxyPort, resp.Status.ProxyPID)
		}
		if resp.Status.Sessions != 1 {
			t.Fatalf("status sessions = %d, want 1", resp.Status.Sessions)
		}
	})

	t.Run("kill", func(t *testing.T) {
		resp, err := c.Kill(t.Context(), true)
		if err != nil {
			t.Fatalf("kill: %v", err)
		}
		if !resp.Kill {
			t.Fatalf("kill resp = %+v", resp)
		}
	})

	t.Run("shadow", func(t *testing.T) {
		resp, err := c.Shadow(t.Context(), true)
		if err != nil {
			t.Fatalf("shadow: %v", err)
		}
		if !resp.Shadow {
			t.Fatalf("shadow resp = %+v", resp)
		}
	})

	t.Run("unknown-op", func(t *testing.T) {
		if _, err := c.call(t.Context(), Op("bogus"), EmptyRequest{}, 2*time.Second); err == nil {
			t.Fatal("unknown op unexpectedly succeeded")
		}
	})
}

type gatedRuntimeReadiness struct {
	entered   chan struct{}
	release   chan error
	published atomic.Bool
}

func (r *gatedRuntimeReadiness) BeforeReady(ctx context.Context) error {
	close(r.entered)
	select {
	case <-ctx.Done():
		return ctx.Err()
	case err := <-r.release:
		return err
	}
}

func (r *gatedRuntimeReadiness) AfterReady(err error) { r.published.Store(err == nil) }

func (r *gatedRuntimeReadiness) Published() bool { return r.published.Load() }

func TestRuntimeHealthAvailableBeforePublication(t *testing.T) {
	shortHome(t)
	server, err := NewServer(testDaemonRole(t))
	if err != nil {
		t.Fatalf("NewServer: %v", err)
	}
	server.log = quietLogger(t)
	server.spawnProxy = func(func(proc.Record) error) error { return nil }
	readiness := &gatedRuntimeReadiness{entered: make(chan struct{}), release: make(chan error, 1)}
	server.readiness = readiness
	startServerSocket(t, server)
	select {
	case <-readiness.entered:
	case <-time.After(time.Second):
		t.Fatal("runtime readiness did not start")
	}

	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	starting, err := client.RuntimeHealth(t.Context())
	if err != nil {
		t.Fatalf("pre-ready runtime health: %v", err)
	}
	if starting.RuntimeBuild != version.String() || starting.RuntimeProtocol != int(wire.ProtocolVersion) ||
		starting.PID <= 1 || starting.ProcessGeneration == "" || starting.Ready ||
		starting.State != "healthy" || starting.Draining || starting.Busy {
		t.Fatalf("pre-ready runtime health = %+v", starting)
	}

	readiness.release <- nil
	if err := client.WaitReady(t.Context(), time.Second); err != nil {
		t.Fatalf("WaitReady after publication: %v", err)
	}
	ready, err := client.RuntimeHealth(t.Context())
	if err != nil {
		t.Fatalf("published runtime health: %v", err)
	}
	if !ready.Ready || ready.ProcessGeneration != starting.ProcessGeneration {
		t.Fatalf("published runtime health = %+v, starting = %+v", ready, starting)
	}
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
			client := NewClient()
			defer client.Close()
			resp, err := client.Mint(t.Context())
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
	srv, err := NewServer(testDaemonRole(t))
	if err != nil {
		t.Fatalf("NewServer: %v", err)
	}
	srv.log = quietLogger(t)
	srv.spawnProxy = func(func(proc.Record) error) error { return nil }
	srv.mintTimeout = 200 * time.Millisecond
	startServer(t, srv)

	// OpMint must not hang past the ready timeout, and with no proxy port known
	// it must reply with a graceful error rather than wedging the daemon.
	start := time.Now()
	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	resp, err := client.Mint(t.Context())
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
	if health, err := client.RuntimeHealth(t.Context()); err != nil || health.RuntimeBuild != version.String() {
		t.Fatalf("daemon wedged after a fail-open mint: health=%+v err=%v", health, err)
	}
}

func TestServerSameReleaseDoesNotReplaceLivePeer(t *testing.T) {
	f := &fakeProxy{port: 50800, pid: 11}
	first := newServerWithProxy(t, f)
	startServer(t, first)

	second, err := NewServer(testDaemonRole(t))
	if err != nil {
		t.Fatalf("NewServer: %v", err)
	}
	second.log = quietLogger(t)
	second.spawnProxy = func(func(proc.Record) error) error { return nil }
	if err := second.Run(context.Background()); err != nil {
		t.Fatalf("same-release contender: %v", err)
	}
	client := NewClient()
	t.Cleanup(func() { _ = client.Close() })
	if health, err := client.RuntimeHealth(t.Context()); err != nil || health.PID == 0 {
		t.Fatalf("incumbent unavailable after contender: health=%+v err=%v", health, err)
	}
}

func TestServerRejectsWrongWireBuildBeforeDispatch(t *testing.T) {
	server := newServerWithProxy(t, &fakeProxy{port: 50810, pid: 12})
	startServer(t, server)
	client, err := wire.NewClient(t.Context(), wire.ClientConfig{
		Dial: wire.UnixDialer(server.socket), WireBuild: "cc-squash.control.wrong",
	})
	if err != nil {
		t.Fatalf("wrong business build handshake: %v", err)
	}
	defer client.Close()
	result, err := client.Call(t.Context(), wire.Op(OpStatus), "", []byte(`{}`))
	if err != nil {
		t.Fatalf("wrong business build call: %v", err)
	}
	if result.Outcome != wire.Rejected || result.Response.Reason != wire.ErrBuildMismatch.Error() {
		t.Fatalf("wrong business build result = %#v", result)
	}
}

func TestActivationAcknowledgesRetiredProxyReceiptAfterDerivedStateCleanup(t *testing.T) {
	shortHome(t)
	if err := paths.EnsureStateDir(); err != nil {
		t.Fatalf("ensure state: %v", err)
	}
	if err := WriteStatus(StatusSnapshot{SchemaVersion: StatusSchemaVersion, Version: "retired"}); err != nil {
		t.Fatalf("seed status: %v", err)
	}
	if err := WritePort(50999); err != nil {
		t.Fatalf("seed port: %v", err)
	}
	boot, err := proc.BootID()
	if err != nil {
		t.Fatalf("boot identity: %v", err)
	}
	store := &proc.FileStore{Path: paths.ProcessStorePath()}
	if err := store.Add(t.Context(), proc.Record{
		RecoveryClass: proc.RecoveryTask,
		PID:           2_000_000_000, StartTime: "retired", Boot: boot, Comm: "ccs-proxy",
		Generation: "retired-generation",
	}); err != nil {
		t.Fatalf("seed retired proxy record: %v", err)
	}
	server, err := NewServer(testDaemonRole(t))
	if err != nil {
		t.Fatalf("NewServer: %v", err)
	}
	server.log = quietLogger(t)
	server.spawnProxy = func(func(proc.Record) error) error { return nil }
	startServer(t, server)
	deadline := time.Now().Add(3 * time.Second)
	for {
		_, statusErr := os.Stat(paths.StatusPath())
		_, portErr := os.Stat(paths.PortFilePath())
		if os.IsNotExist(statusErr) && os.IsNotExist(portErr) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("retired derived state was not cleared: status=%v port=%v", statusErr, portErr)
		}
		time.Sleep(10 * time.Millisecond)
	}
	if _, err := os.Stat(paths.StatusPath()); !os.IsNotExist(err) {
		t.Fatalf("retired status still present: %v", err)
	}
	if _, err := os.Stat(paths.PortFilePath()); !os.IsNotExist(err) {
		t.Fatalf("retired port still present: %v", err)
	}
	var page proc.ReapReceiptPage
	for {
		page, err = store.LoadReapReceipts(
			t.Context(), proc.RecoveryTask, proc.ReapReceiptCursor{}, proc.ReapReceiptPageLimit,
		)
		if err != nil {
			t.Fatalf("read receipt ledger: %v", err)
		}
		if page.Floor.Sequence == 1 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("receipt acknowledgement did not commit: %+v", page)
		}
		time.Sleep(10 * time.Millisecond)
	}
	if len(page.Receipts) != 0 || page.Floor.Sequence != 1 {
		t.Fatalf("receipt page after activation = %+v", page)
	}
}

func TestServerStatusFileWritten(t *testing.T) {
	f := &fakeProxy{port: 50900, pid: 13}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	// onRegister writes status-v1.json atomically once the proxy registers; poll for
	// it (the bring-up is asynchronous).
	deadline := time.Now().Add(3 * time.Second)
	for {
		snap, err := ReadStatus()
		if err == nil && snap.ProxyPort == 50900 {
			if snap.ProxyPID != f.pid {
				t.Fatalf("status-v1.json pid = %d, want %d", snap.ProxyPID, f.pid)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("status-v1.json never reflected the proxy port (last err %v)", err)
		}
		time.Sleep(10 * time.Millisecond)
	}

	// The published port-file matches.
	if port, err := ReadPort(); err != nil || port != 50900 {
		t.Fatalf("port-file = %d (err %v), want 50900", port, err)
	}
}

// TestServerKillReflectedInStatusFile is the BUG A regression: a kill/shadow
// toggle must refresh status-v1.json so out-of-process readers (`ccs status`, `ccs
// kill status`, both via ReadStatus) see the live value, not a stale snapshot
// from the last register.
func TestServerKillReflectedInStatusFile(t *testing.T) {
	f := &fakeProxy{port: 51000, pid: 17}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)
	c := NewClient()
	t.Cleanup(func() { _ = c.Close() })

	// Wait for the cold-start register so status-v1.json exists with kill=off,
	// then drain the register's effect by reading the first status.
	deadline := time.Now().Add(3 * time.Second)
	for {
		if snap, err := ReadStatus(); err == nil && snap.ProxyPort == 51000 {
			if snap.Kill || snap.Shadow {
				t.Fatalf("cold status-v1.json already toggled on: %+v", snap)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("status-v1.json never reflected the cold-start proxy port")
		}
		time.Sleep(10 * time.Millisecond)
	}

	if _, err := c.Kill(t.Context(), true); err != nil {
		t.Fatalf("kill on: %v", err)
	}
	if snap, err := ReadStatus(); err != nil || !snap.Kill {
		t.Fatalf("status-v1.json kill = %v (err %v) after `kill on`, want true", snap.Kill, err)
	}

	if _, err := c.Shadow(t.Context(), true); err != nil {
		t.Fatalf("shadow on: %v", err)
	}
	if snap, err := ReadStatus(); err != nil || !snap.Shadow {
		t.Fatalf("status-v1.json shadow = %v (err %v) after `shadow on`, want true", snap.Shadow, err)
	}

	if _, err := c.Kill(t.Context(), false); err != nil {
		t.Fatalf("kill off: %v", err)
	}
	snap, err := ReadStatus()
	if err != nil {
		t.Fatalf("read status after kill off: %v", err)
	}
	if snap.Kill {
		t.Fatalf("status-v1.json kill = true after `kill off`, want false: %+v", snap)
	}
	// Shadow stays on — the toggles are independent.
	if !snap.Shadow {
		t.Fatalf("status-v1.json shadow flipped off when only kill was toggled: %+v", snap)
	}
}

// TestServerShutdownStepsDownProxy is the BUG B regression: an intentional
// daemon shutdown (context cancellation, authorized stop, or SIGTERM)
// must send the proxy an explicit seam Shutdown frame so `ccs stop` takes the
// proxy down with the daemon — not leave it orphaned on a bare seam drop.
func TestServerShutdownStepsDownProxy(t *testing.T) {
	f := &fakeProxy{port: 51100, pid: 19}
	srv := newServerWithProxy(t, f)
	cancel := startServer(t, srv)

	// Wait for the proxy to register so the seam has a live connection to push the
	// shutdown frame to.
	deadline := time.Now().Add(3 * time.Second)
	for {
		if snap, err := ReadStatus(); err == nil && snap.ProxyPort == 51100 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("proxy never registered before shutdown")
		}
		time.Sleep(10 * time.Millisecond)
	}

	seen := f.stepDownOnShutdown(t)
	cancel() // intentional daemon shutdown (the `ccs stop` / SIGTERM teardown)

	select {
	case <-seen:
	case <-time.After(3 * time.Second):
		t.Fatal("proxy never received a Shutdown frame on intentional daemon shutdown — it would orphan")
	}
}
