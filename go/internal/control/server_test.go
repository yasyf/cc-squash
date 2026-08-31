package control

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/cc-squash/go/internal/supervisor"
	"github.com/yasyf/daemonkit"
)

// quietLogger discards daemon diagnostics so a test run stays clean.
func quietLogger(t *testing.T) *log.Logger {
	t.Helper()
	return log.New(io.Discard, "", 0)
}

// fakeProxy stands in for the Rust ccs-proxy child. It owns the child end of a
// real socketpair — the channel a daemonkit ChannelHandoff spawn would inherit
// at fd 3 — sends one register frame, and then reads the control frames the
// daemon pushes.
type fakeProxy struct {
	port    int
	mcpPort int
	version string

	channel net.Conn
	child   net.Conn
	reader  *bufio.Reader

	mu      sync.Mutex
	stopped bool
}

func (f *fakeProxy) PID() int { return os.Getpid() }

func (f *fakeProxy) Conn() (net.Conn, error) { return f.channel, nil }

func (f *fakeProxy) Stop(context.Context) (daemonkit.Exit, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if !f.stopped {
		f.stopped = true
		_ = f.child.Close()
	}
	return daemonkit.Exit{}, nil
}

// attach builds the handoff channel pair and fills in the defaults a registered
// child must carry. The version is the one this daemon supervises against, so
// the supervisor reads the child as steady state and never replaces it.
func (f *fakeProxy) attach(t *testing.T) {
	t.Helper()
	f.channel, f.child = socketPair(t)
	f.reader = bufio.NewReader(f.child)
	if f.mcpPort == 0 {
		f.mcpPort = f.port + 1
	}
	if f.version == "" {
		f.version = supervisor.ProxyVersion()
	}
}

func (f *fakeProxy) register() error {
	frame, err := proxyseam.Encode(proxyseam.Register{
		Type: proxyseam.MsgRegister, Protocol: proxyseam.ProtocolVersion,
		Port: f.port, MCPPort: f.mcpPort, Version: f.version, PID: f.PID(),
	})
	if err != nil {
		return err
	}
	_, err = f.child.Write(frame)
	return err
}

// readFrame blocks until the daemon pushes one control frame and returns it
// decoded.
func (f *fakeProxy) readFrame() (any, error) {
	line, err := f.reader.ReadBytes('\n')
	if err != nil {
		return nil, err
	}
	return proxyseam.Decode(line[:len(line)-1])
}

func (f *fakeProxy) readMint() (proxyseam.Mint, error) {
	frame, err := f.readFrame()
	if err != nil {
		return proxyseam.Mint{}, err
	}
	mint, ok := frame.(proxyseam.Mint)
	if !ok {
		return proxyseam.Mint{}, fmt.Errorf("proxy saw %T, want proxyseam.Mint", frame)
	}
	return mint, nil
}

// awaitShutdown drains control frames until a Shutdown arrives, then drops the
// channel — the real proxy stepping down. The returned channel closes once that
// frame has been observed.
func (f *fakeProxy) awaitShutdown() <-chan struct{} {
	seen := make(chan struct{})
	go func() {
		for {
			frame, err := f.readFrame()
			if err != nil {
				return
			}
			if _, ok := frame.(proxyseam.Shutdown); ok {
				_ = f.child.Close()
				close(seen)
				return
			}
		}
	}()
	return seen
}

// socketPair returns the parent and child ends of one AF_UNIX stream pair.
func socketPair(t *testing.T) (parent, child net.Conn) {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		t.Fatalf("socketpair: %v", err)
	}
	ends := make([]net.Conn, len(fds))
	for i, fd := range fds {
		file := os.NewFile(uintptr(fd), fmt.Sprintf("proxy-channel-%d", i))
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

// newServerWithProxy builds a daemon whose spawn seam hands the seam the given
// fake proxy's channel, under an isolated home.
func newServerWithProxy(t *testing.T, f *fakeProxy) *Server {
	t.Helper()
	shortHome(t)
	srv := newTestServer(t)
	if f == nil {
		return srv
	}
	f.attach(t)
	srv.spawnProxy = func(context.Context) (proxyChild, error) {
		if err := f.register(); err != nil {
			return nil, err
		}
		return f, nil
	}
	return srv
}

// newTestServer builds a quiet daemon. The caller has already isolated the home.
func newTestServer(t *testing.T) *Server {
	t.Helper()
	srv, err := NewServer()
	if err != nil {
		t.Fatalf("NewServer: %v", err)
	}
	srv.log = quietLogger(t)
	return srv
}

func newTestClient(t *testing.T) *Client {
	t.Helper()
	client, err := NewClient()
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	t.Cleanup(func() { _ = client.Close() })
	return client
}

// startServer runs srv.Run and waits for the business lane to dispatch.
func startServer(t *testing.T, srv *Server) context.CancelFunc {
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
	probe := newTestClient(t)
	deadline := time.Now().Add(5 * time.Second)
	for {
		if _, err := probe.Status(t.Context()); err == nil {
			return cancel
		} else if time.Now().After(deadline) {
			t.Fatalf("daemon never dispatched: %v", err)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// awaitRegisteredStatus polls the status mirror until the cold-started proxy's
// port lands in it.
func awaitRegisteredStatus(t *testing.T, port int) StatusSnapshot {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		snapshot, err := ReadStatus()
		if err == nil && snapshot.ProxyPort == port {
			return snapshot
		}
		if time.Now().After(deadline) {
			t.Fatalf("status-v1.json never reflected proxy port %d (last err %v)", port, err)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestServerColdStartMint(t *testing.T) {
	f := &fakeProxy{port: 50516}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	client := newTestClient(t)
	response, err := client.Mint(t.Context())
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	if !response.OK || response.Port != 50516 || response.Token == "" {
		t.Fatalf("mint = %+v", response)
	}
	snapshot := awaitRegisteredStatus(t, response.Port)
	if snapshot.ProxyPID != f.PID() {
		t.Fatalf("status pid = %d, want %d", snapshot.ProxyPID, f.PID())
	}
	mint, err := f.readMint()
	if err != nil {
		t.Fatalf("proxy read mint: %v", err)
	}
	if mint.Token != response.Token {
		t.Fatalf("proxy saw token %q, daemon replied %q", mint.Token, response.Token)
	}
}

// TestServerMintReturnsMCPPort pins the second listener's port through the whole
// path: the register frame carries it, the daemon records it, and one mint
// round-trip surfaces it so `ccs run` can build the --mcp-config URL.
func TestServerMintReturnsMCPPort(t *testing.T) {
	f := &fakeProxy{port: 50516, mcpPort: 50517}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	client := newTestClient(t)
	response, err := client.Mint(t.Context())
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	if response.MCPPort != 50517 {
		t.Fatalf("mint mcp_port = %d, want 50517", response.MCPPort)
	}
	if _, err := f.readMint(); err != nil {
		t.Fatalf("drain mint: %v", err)
	}
	status, err := client.Status(t.Context())
	if err != nil {
		t.Fatalf("status: %v", err)
	}
	if status.Status.ProxyMCPort != 50517 {
		t.Fatalf("status proxy_mcp_port = %d, want 50517", status.Status.ProxyMCPort)
	}
}

// TestServerGcForwardsFrame is the `ccs gc` dispatch assertion: an OpGc request
// forwards exactly one gc frame to the proxy, which sweeps its ref store.
func TestServerGcForwardsFrame(t *testing.T) {
	f := &fakeProxy{port: 50520}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)
	awaitRegisteredStatus(t, f.port)

	client := newTestClient(t)
	response, err := client.Gc(t.Context())
	if err != nil {
		t.Fatalf("gc: %v", err)
	}
	if !response.OK {
		t.Fatalf("gc not OK: %+v", response)
	}
	frame, err := f.readFrame()
	if err != nil {
		t.Fatalf("read gc frame: %v", err)
	}
	if _, ok := frame.(proxyseam.Gc); !ok {
		t.Fatalf("proxy saw %T, want proxyseam.Gc", frame)
	}
}

// TestServerGcWithoutProxyReportsNothingToSweep pins the fail-open half: with no
// data plane connected the sweep is a benign refusal, not a daemon fault.
func TestServerGcWithoutProxyReportsNothingToSweep(t *testing.T) {
	shortHome(t)
	srv := newTestServer(t)
	srv.spawnProxy = func(context.Context) (proxyChild, error) {
		return nil, supervisor.ErrChildUnavailable
	}
	startServer(t, srv)

	response, err := newTestClient(t).Gc(t.Context())
	if err != nil {
		t.Fatalf("gc: %v", err)
	}
	if response.OK || response.Error == "" {
		t.Fatalf("gc with no data plane = %+v, want a benign refusal", response)
	}
}

// TestServerMintCarriesConfig is the seam's config contract: handleMint pushes
// the config loaded from config.toml, so the proxy mints each session with the
// user's relay knobs rather than engine defaults.
func TestServerMintCarriesConfig(t *testing.T) {
	f := &fakeProxy{port: 51200}
	srv := newServerWithProxy(t, f)
	writeTestConfig(t, "[economics]\nnpv_floor = 0.25\n")
	startServer(t, srv)

	if _, err := newTestClient(t).Mint(t.Context()); err != nil {
		t.Fatalf("mint: %v", err)
	}
	mint, err := f.readMint()
	if err != nil {
		t.Fatalf("read mint frame: %v", err)
	}
	if string(mint.Config) != `{"economics":{"npv_floor":0.25}}` {
		t.Fatalf("mint config = %s, want the loaded config.toml", mint.Config)
	}
}

// writeTestConfig writes config.toml under the already-isolated test home so the
// daemon's startup config.Load reads it.
func writeTestConfig(t *testing.T, toml string) {
	t.Helper()
	if err := os.WriteFile(paths.ConfigPath(), []byte("schema_version = 1\n"+toml), 0o600); err != nil {
		t.Fatalf("write config.toml: %v", err)
	}
}

func TestServerProtocolRoundTrips(t *testing.T) {
	f := &fakeProxy{port: 50600}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)
	client := newTestClient(t)

	t.Run("status", func(t *testing.T) {
		if _, err := client.Mint(t.Context()); err != nil {
			t.Fatalf("mint: %v", err)
		}
		if _, err := f.readMint(); err != nil {
			t.Fatalf("drain mint: %v", err)
		}
		response, err := client.Status(t.Context())
		if err != nil {
			t.Fatalf("status: %v", err)
		}
		if response.Status == nil {
			t.Fatal("status snapshot missing")
		}
		if response.Status.ProxyPort != 50600 || response.Status.ProxyPID != f.PID() {
			t.Fatalf("status proxy = port %d pid %d", response.Status.ProxyPort, response.Status.ProxyPID)
		}
		if response.Status.Sessions != 1 {
			t.Fatalf("status sessions = %d, want 1", response.Status.Sessions)
		}
	})

	t.Run("kill", func(t *testing.T) {
		response, err := client.Kill(t.Context(), true)
		if err != nil {
			t.Fatalf("kill: %v", err)
		}
		if !response.Kill {
			t.Fatalf("kill = %+v", response)
		}
	})

	t.Run("shadow", func(t *testing.T) {
		response, err := client.Shadow(t.Context(), true)
		if err != nil {
			t.Fatalf("shadow: %v", err)
		}
		if !response.Shadow {
			t.Fatalf("shadow = %+v", response)
		}
	})

	t.Run("unknown-op", func(t *testing.T) {
		if _, err := client.call(t.Context(), Op("bogus"), EmptyRequest{}, 2*time.Second); err == nil {
			t.Fatal("unknown op unexpectedly succeeded")
		}
	})
}

func TestServerMintDemux(t *testing.T) {
	srv := newServerWithProxy(t, &fakeProxy{port: 50700})
	startServer(t, srv)

	const sessions = 4
	tokens := make(chan string, sessions)
	var wg sync.WaitGroup
	for range sessions {
		wg.Add(1)
		go func() {
			defer wg.Done()
			client := newTestClient(t)
			response, err := client.Mint(t.Context())
			if err != nil {
				t.Errorf("mint: %v", err)
				return
			}
			tokens <- response.Token
		}()
	}
	wg.Wait()
	close(tokens)

	seen := map[string]bool{}
	for token := range tokens {
		if token == "" {
			t.Fatal("empty token")
		}
		if seen[token] {
			t.Fatalf("duplicate token %q", token)
		}
		seen[token] = true
	}
	if len(seen) != sessions {
		t.Fatalf("got %d unique tokens, want %d", len(seen), sessions)
	}
}

func TestServerSeamFailOpen(t *testing.T) {
	shortHome(t)
	srv := newTestServer(t)
	srv.spawnProxy = func(context.Context) (proxyChild, error) {
		return nil, supervisor.ErrChildUnavailable
	}
	srv.mintTimeout = 200 * time.Millisecond
	startServer(t, srv)

	client := newTestClient(t)
	started := time.Now()
	response, err := client.Mint(t.Context())
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	if elapsed := time.Since(started); elapsed > 2*time.Second {
		t.Fatalf("mint took %s; the ready wait did not bound it", elapsed)
	}
	if response.OK {
		t.Fatalf("mint reported OK with no proxy port: %+v", response)
	}
	if response.Error == "" {
		t.Fatal("mint failed open but gave no error message")
	}
	if _, err := client.Status(t.Context()); err != nil {
		t.Fatalf("daemon wedged after a fail-open mint: %v", err)
	}
}

// TestSecondServeRefusesLiveIncumbent pins the singleton: a contender that finds
// a live daemon on the socket refuses outright and leaves the incumbent serving,
// rather than evicting it or binding beside it.
func TestSecondServeRefusesLiveIncumbent(t *testing.T) {
	srv := newServerWithProxy(t, &fakeProxy{port: 50800})
	startServer(t, srv)

	contender := newTestServer(t)
	contender.spawnProxy = func(context.Context) (proxyChild, error) {
		return nil, supervisor.ErrChildUnavailable
	}
	if err := contender.Run(context.Background()); !errors.Is(err, daemonkit.ErrBusy) {
		t.Fatalf("contender Run = %v, want ErrBusy", err)
	}
	if _, err := newTestClient(t).Status(t.Context()); err != nil {
		t.Fatalf("incumbent unavailable after contender: %v", err)
	}
}

// TestServerRejectsWrongWireBuildBeforeDispatch pins admission: a client whose
// schema fingerprint differs never reaches Handle, so a skewed build cannot mint
// against this daemon.
func TestServerRejectsWrongWireBuildBeforeDispatch(t *testing.T) {
	srv := newServerWithProxy(t, &fakeProxy{port: 50810})
	startServer(t, srv)

	skewed := Identity()
	skewed.Schemas = []daemonkit.Schema{daemonkit.Schema("com.yasyf.cc-squash.control/skewed/v1")}
	client, err := daemonkit.Open(skewed)
	if err != nil {
		t.Fatalf("open skewed client: %v", err)
	}
	business := client.Business()
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		defer cancel()
		_ = business.Close(ctx)
	})
	if reply, err := business.Call(t.Context(), string(OpStatus), []byte(`{}`)); err == nil {
		t.Fatalf("skewed wire build dispatched: %+v", reply)
	}
	if status, err := newTestClient(t).Status(t.Context()); err != nil || status.Status.Sessions != 0 {
		t.Fatalf("incumbent after a rejected build = %+v, err = %v", status, err)
	}
}

// TestClearRetiredProxyStateRemovesDerivedFiles pins the reclaim path's cleanup:
// a daemon that inherits a prior generation's children drops the port-file and
// status mirror that described the retired proxy, so no reader is served a
// snapshot of a process that is gone.
func TestClearRetiredProxyStateRemovesDerivedFiles(t *testing.T) {
	shortHome(t)
	if err := WriteStatus(StatusSnapshot{SchemaVersion: StatusSchemaVersion, Version: "retired"}); err != nil {
		t.Fatalf("seed status: %v", err)
	}
	if err := WritePort(50999); err != nil {
		t.Fatalf("seed port: %v", err)
	}
	if err := clearRetiredProxyState(); err != nil {
		t.Fatalf("clear retired proxy state: %v", err)
	}
	if _, err := os.Stat(paths.StatusPath()); !os.IsNotExist(err) {
		t.Fatalf("retired status still present: %v", err)
	}
	if _, err := os.Stat(paths.PortFilePath()); !os.IsNotExist(err) {
		t.Fatalf("retired port still present: %v", err)
	}
	if err := clearRetiredProxyState(); err != nil {
		t.Fatalf("clearing already-absent derived state: %v", err)
	}
}

func TestServerStatusFileWritten(t *testing.T) {
	f := &fakeProxy{port: 50900}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)

	snapshot := awaitRegisteredStatus(t, f.port)
	if snapshot.ProxyPID != f.PID() {
		t.Fatalf("status-v1.json pid = %d, want %d", snapshot.ProxyPID, f.PID())
	}
	if port, err := ReadPort(); err != nil || port != f.port {
		t.Fatalf("port-file = %d (err %v), want %d", port, err, f.port)
	}
}

// TestServerKillReflectedInStatusFile pins the out-of-process view: a kill or
// shadow toggle refreshes status-v1.json, so `ccs status` and `ccs kill status`
// read the live value rather than the last register's snapshot.
func TestServerKillReflectedInStatusFile(t *testing.T) {
	f := &fakeProxy{port: 51000}
	srv := newServerWithProxy(t, f)
	startServer(t, srv)
	client := newTestClient(t)

	if cold := awaitRegisteredStatus(t, f.port); cold.Kill || cold.Shadow {
		t.Fatalf("cold status-v1.json already toggled on: %+v", cold)
	}
	if _, err := client.Kill(t.Context(), true); err != nil {
		t.Fatalf("kill on: %v", err)
	}
	if snapshot, err := ReadStatus(); err != nil || !snapshot.Kill {
		t.Fatalf("status-v1.json kill = %v (err %v) after `kill on`", snapshot.Kill, err)
	}
	if _, err := client.Shadow(t.Context(), true); err != nil {
		t.Fatalf("shadow on: %v", err)
	}
	if snapshot, err := ReadStatus(); err != nil || !snapshot.Shadow {
		t.Fatalf("status-v1.json shadow = %v (err %v) after `shadow on`", snapshot.Shadow, err)
	}
	if _, err := client.Kill(t.Context(), false); err != nil {
		t.Fatalf("kill off: %v", err)
	}
	snapshot, err := ReadStatus()
	if err != nil {
		t.Fatalf("read status after kill off: %v", err)
	}
	if snapshot.Kill {
		t.Fatalf("status-v1.json kill = true after `kill off`: %+v", snapshot)
	}
	if !snapshot.Shadow {
		t.Fatalf("status-v1.json shadow flipped off when only kill was toggled: %+v", snapshot)
	}
}

// TestServerShutdownStepsDownProxy pins the teardown: an intentional daemon
// shutdown sends the proxy an explicit seam Shutdown frame, so `ccs stop` takes
// the data plane down with the daemon instead of orphaning it on a bare drop.
func TestServerShutdownStepsDownProxy(t *testing.T) {
	f := &fakeProxy{port: 51100}
	srv := newServerWithProxy(t, f)
	cancel := startServer(t, srv)
	awaitRegisteredStatus(t, f.port)

	seen := f.awaitShutdown()
	cancel()
	select {
	case <-seen:
	case <-time.After(5 * time.Second):
		t.Fatal("proxy never received a Shutdown frame on intentional daemon shutdown")
	}
}

func TestCloseProductJoinIsBounded(t *testing.T) {
	server := &Server{}
	server.wg.Add(1)
	defer server.wg.Done()
	ctx, cancel := context.WithTimeout(t.Context(), 20*time.Millisecond)
	defer cancel()
	if err := server.closeProduct(ctx); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("closeProduct = %v, want deadline", err)
	}
}
