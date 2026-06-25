package control

import (
	"context"
	"encoding/json"
	"errors"
	"log"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"strconv"
	"sync"
	"syscall"
	"time"

	"github.com/yasyf/cc-squash/go/internal/config"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/cc-squash/go/internal/supervisor"
	"github.com/yasyf/fusekit/proc"
	"github.com/yasyf/fusekit/version"
)

// evictTimeout bounds how long a starting daemon waits for a version-skewed peer
// to release the socket after being told to step down.
const evictTimeout = 5 * time.Second

// mintReadyTimeout bounds how long OpMint waits for a cold-started proxy to
// register before it falls open and replies with whatever it knows.
const mintReadyTimeout = 3 * time.Second

// proxyStartupGrace bounds how long bringUp waits for the first proxy to
// register before starting the supervise loop anyway — long enough for a normal
// bind+connect+register (sub-second), so the first tick never races a healthy
// cold start into a spurious respawn.
const proxyStartupGrace = 5 * time.Second

// proxyShutdownGrace bounds how long an intentional daemon shutdown waits for the
// supervised proxy to step down after the seam shutdown frame, before the daemon
// returns and the seam Close drops the connection.
const proxyShutdownGrace = 3 * time.Second

// Server is the cc-squash control-plane daemon: it binds the control socket
// (single-entrant under a flock), binds the proxy.sock seam, spawns and
// supervises the Rust ccs-proxy data plane, and answers the CLI's
// newline-delimited JSON requests.
type Server struct {
	socket    string
	proxySock string
	log       *log.Logger

	seam   *proxyseam.Server
	sup    *proc.Supervisor
	policy *supervisor.ProxyPolicy

	// spawnProxy overrides the detached ccs-proxy spawn; nil execs the real
	// binary. Tests inject a fake child that connects to proxy.sock and registers
	// through the SAME proc.Spawn.Override seam, so no real proxy is exec'd.
	spawnProxy func() error

	// mintTimeout bounds OpMint's wait for a cold-started proxy to register; zero
	// means mintReadyTimeout. Tests shrink it to pin the fail-open path fast.
	mintTimeout time.Duration

	// triggerShutdown cancels serve's context, ending the daemon. Set once in
	// serve before the accept loop starts; the go-statement that spawns each
	// handler establishes the happens-before, so handlers read it without a lock.
	triggerShutdown context.CancelFunc

	// relayConfig is the seam JSON parsed from config.toml once at daemon start
	// and pushed verbatim with every mint. Set in serve before the accept loop or
	// bring-up spawns, so the go-statements establish the happens-before and the
	// mint/repush readers take no lock. A load error fails open to {} so a bad
	// config never blocks minting.
	relayConfig json.RawMessage

	// wg tracks every daemon goroutine (the startup bring-up, the supervise loop,
	// each connection handler); serve Waits on it before returning.
	wg sync.WaitGroup

	// proxyReady is closed once the proxy registers, so OpMint can wait for a
	// cold-started data plane rather than failing the first mint.
	proxyReady chan struct{}
	readyOnce  sync.Once

	mu        sync.Mutex
	tokens    map[Token]struct{}
	proxyPort int
	mcpPort   int
	proxyPID  int
	kill      bool
	shadow    bool
}

// NewServer returns a control-plane daemon bound to the default socket paths,
// logging to stderr.
func NewServer() *Server {
	return &Server{
		socket:     paths.SocketPath(),
		proxySock:  paths.ProxySocketPath(),
		log:        log.New(os.Stderr, "[cc-squash] ", log.LstdFlags),
		proxyReady: make(chan struct{}),
		tokens:     map[Token]struct{}{},
	}
}

// Run is the entry point for `ccs daemon`. It blocks until the process is
// signalled or told to step down over the socket.
func (s *Server) Run(ctx context.Context) error {
	return s.serve(ctx)
}

func (s *Server) serve(ctx context.Context) error {
	if err := paths.EnsureStateDir(); err != nil {
		return err
	}
	ln, lock, err := s.listen()
	if err != nil {
		return err
	}
	// The flock on lock is the cross-process guarantee that only this daemon may
	// stale-check, remove, bind, or unlink the socket. It must outlive the
	// listener (Close releases it), so this defer is registered first and runs
	// last.
	defer func() { _ = lock.Close() }()
	// closeListener unlinks the socket exactly once. *net.UnixListener.Close
	// unlinks by PATH and is NOT idempotent: a second Close would delete a
	// successor daemon's freshly-bound socket. The sync.Once pins the unlink to
	// the first close, at ctx-cancel time. No explicit os.Remove for the same
	// reason; the lock file is never removed either.
	var closeOnce sync.Once
	closeListener := func() { closeOnce.Do(func() { _ = ln.Close() }) }
	defer closeListener()

	ctx, stop := signal.NotifyContext(ctx, syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	// stop cancels ctx, so it doubles as the over-the-socket shutdown trigger
	// (OpShutdown). Set before the accept loop spawns any handler.
	s.triggerShutdown = stop

	// Load the relay config once, before any goroutine that pushes a mint spawns.
	// Fail-open: a missing or malformed config.toml degrades to {} (engine
	// defaults) and is logged, never blocks minting.
	cfg, err := config.Load()
	if err != nil {
		s.log.Printf("config: load failed, pushing engine defaults: %v", err)
		cfg = json.RawMessage("{}")
	}
	s.relayConfig = cfg

	// Bind the proxy.sock seam BEFORE spawning the child, so the child has
	// something to connect to the instant it binds its TCP port.
	seam, err := proxyseam.NewServer(s.log)
	if err != nil {
		return err
	}
	s.seam = seam
	defer func() { _ = seam.Close() }()
	s.policy = supervisor.NewProxyPolicy(seam, s.repushTokens, s.log)

	s.log.Printf("daemon %s started; socket=%s", version.String(), s.socket)

	// Post-bind latency rule: bind and start accepting FIRST so Health/Mint
	// answer instantly, then defer the heavy bring-up (spawn the proxy, await its
	// register, build the supervisor, drive the supervise loop) to a goroutine
	// launched after the listener is up.
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		s.bringUp(ctx)
	}()

	// Break the accept loop on shutdown.
	go func() {
		<-ctx.Done()
		closeListener()
	}()
	for {
		conn, err := ln.Accept()
		if err != nil {
			if ctx.Err() != nil || errors.Is(err, net.ErrClosed) {
				break
			}
			s.log.Printf("accept: %v", err)
			time.Sleep(100 * time.Millisecond)
			continue
		}
		s.wg.Add(1)
		go func() { defer s.wg.Done(); s.handle(ctx, conn) }()
	}

	s.wg.Wait()
	s.shutdownProxy()
	s.log.Printf("daemon stopped")
	return nil
}

// shutdownProxy gracefully stops the supervised proxy on an intentional daemon
// shutdown: it sends the seam shutdown frame and waits (bounded) for the child to
// step down, so `ccs stop` / SIGTERM takes the proxy down with the daemon.
//
// Runs only after the supervise loop has quiesced (wg.Wait above), so no Tick
// races a respawn against this teardown, and BEFORE the deferred seam Close — the
// distinguisher that preserves crash fail-open. An EXPLICIT shutdown frame is the
// intentional-stop signal; a bare seam drop (daemon crash, no frame) still leaves
// the proxy serving standalone so live `ccs url` tokens survive until the daemon
// respawns. The wait uses a fresh context because the daemon's own ctx is already
// cancelled by the time teardown runs.
func (s *Server) shutdownProxy() {
	if err := s.policy.Shutdown(context.Background()); err != nil {
		if errors.Is(err, proxyseam.ErrProxyNotConnected) {
			return // no proxy connected; nothing to step down
		}
		s.log.Printf("shutdown proxy: %v", err)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), proxyShutdownGrace)
	defer cancel()
	if !s.policy.WaitGone(ctx, proxyShutdownGrace) {
		s.log.Printf("proxy did not step down within %s; closing seam", proxyShutdownGrace)
	}
}

// bringUp runs the deferred heavy startup off the accept path: it starts the
// seam accept loop (capturing the proxy's register), spawns the data-plane
// child, builds the supervisor, and drives the supervise loop until ctx is
// cancelled.
//
// The supervise loop only starts once the first proxy has registered (or the
// startup grace elapses): the spawn-and-wait here and the loop's revive are two
// spawn entry points, and a tick that fires before the just-spawned proxy
// registers would read it unreachable, spuriously fire ChildDied (clearing
// identity, burning a crash-loop count), and exec a SECOND proxy that binds a
// different ephemeral port and orphans. Waiting on proxyReady collapses the two
// entry points into one. A proxy that never registers falls through after the
// grace to the loop's normal revive/backoff — the genuinely-dead-on-startup
// case the supervisor exists to handle.
func (s *Server) bringUp(ctx context.Context) {
	go s.seam.Start(ctx, s.onRegister)

	spawn := proc.Spawn{
		Socket:    s.proxySock,
		LogPath:   paths.LogPath(),
		Available: s.policy.Registered,
		CanHost:   func() error { return nil },
		Override:  s.spawnProxyChild,
	}
	if err := spawn.EnsureRunning(); err != nil {
		s.log.Printf("spawn proxy: %v", err)
	}
	select {
	case <-s.proxyReady:
	case <-ctx.Done():
		return
	case <-time.After(proxyStartupGrace):
		s.log.Printf("proxy did not register within %s; starting supervision (revive will retry)", proxyStartupGrace)
	}
	s.sup = supervisor.BuildSupervisor(spawn, s.policy, supervisor.ProxyVersion())
	supervisor.SuperviseLoop(ctx, s.sup)
}

// onRegister captures a freshly registered proxy's identity, publishes its port
// (status mirror + port-file), and unblocks any OpMint waiting on the cold
// start. Runs on the seam's accept goroutine.
func (s *Server) onRegister(reg proxyseam.Register) {
	if want := supervisor.ProxyVersion(); reg.Version != want {
		// The registered proxy is not the version this daemon supervises against, so
		// the supervisor will Replace it every tick (it reads any other version as a
		// skewed, on-disk-upgraded child) — the proxy flaps until the operator
		// restarts the daemon so both converge on the on-disk binary.
		s.log.Printf("WARNING: proxy version %q != supervised version %q; the supervisor will keep replacing it — restart the daemon to converge", reg.Version, want)
	}
	s.policy.NoteRegistered(reg)
	s.mu.Lock()
	s.proxyPort = reg.Port
	s.mcpPort = reg.MCPPort
	s.proxyPID = reg.PID
	s.mu.Unlock()
	s.readyOnce.Do(func() { close(s.proxyReady) })
	if err := WritePort(reg.Port); err != nil {
		s.log.Printf("write port-file: %v", err)
	}
	s.publishStatus()
}

// spawnProxyChild is the proc.Spawn.Override: it execs the detached Rust
// ccs-proxy on the seam socket, routed through the spawnProxy seam so tests
// inject a fake child the same way production spawns the real one. The port is
// read dynamically (currentProxyPort): 0 lets the OS assign one on the very
// first start, and the prior registered port is reused on every respawn, so a
// crash-replaced proxy re-binds the SAME port and every outstanding `ccs url`
// token keeps resolving. Returns nil so a started child is awaited by
// EnsureRunning's come-up loop (the child registers, flipping Available true).
func (s *Server) spawnProxyChild() error {
	if s.spawnProxy != nil {
		return s.spawnProxy()
	}
	bin, err := ProxyBinaryPath()
	if err != nil {
		return err
	}
	logFile, err := os.OpenFile(paths.LogPath(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer func() { _ = logFile.Close() }()
	cmd := exec.Command(bin, "--socket", s.proxySock, "--port", strconv.Itoa(s.currentProxyPort()), "--state-dir", paths.StateDir())
	cmd.Stdin = nil
	cmd.Stdout, cmd.Stderr = logFile, logFile
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true} // detach from our session
	if err := cmd.Start(); err != nil {
		return err
	}
	go func() { _ = cmd.Wait() }() // reap so the exited child never strands a zombie
	return nil
}

// currentProxyPort is the port the next spawned proxy must bind: 0 before any
// proxy has registered (OS-assigned on the first start), the prior registered
// port thereafter. Reading it per spawn is what pins a respawned proxy to the
// same port — onRegister captures it once and ChildDied leaves it intact across
// a crash, so the replacement re-binds it and live tokens survive.
func (s *Server) currentProxyPort() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.proxyPort
}

// listen binds the control socket single-entrant under a flock, refusing a live
// same-version peer and evicting a version-skewed one. The flock — held by serve
// for the daemon's lifetime — makes the stale-check/remove/bind sequence
// single-entrant across processes; the cc-squash-specific contention policy is
// the evict closure.
func (s *Server) listen() (net.Listener, *os.File, error) {
	return proc.SingleEntrant{
		Socket:  s.socket,
		Timeout: evictTimeout,
		Evict:   s.evict,
	}.Listen()
}

// evict is the SingleEntrant contention policy: health-probe the peer holding
// the socket. A live same-version peer is a genuine double start — refuse. A
// version-skewed peer is told to step down and waited out — evict. No peer
// answered — make no claim (a free lock binds; a contended one is refused by
// proc as a peer mid-start).
func (s *Server) evict() (bool, error) {
	c := &Client{socket: s.socket}
	resp, err := c.Health()
	if err != nil {
		return false, nil // no live peer answered
	}
	if resp.Version == version.String() {
		return false, errors.New("another cc-squash daemon at the same version is already running")
	}
	s.log.Printf("evicting version-skewed daemon (%s) holding the socket", resp.Version)
	if _, err := c.Shutdown(); err != nil {
		return false, err
	}
	if !c.WaitGone(evictTimeout) {
		return false, errors.New("version-skewed daemon did not release the socket in time")
	}
	return true, nil
}

// handle serves one connection: decode one Request, dispatch it, encode one
// Response. ctx bounds the daemon's lifecycle; the conn deadline independently
// bounds a single slow client.
func (s *Server) handle(ctx context.Context, conn net.Conn) {
	defer func() { _ = conn.Close() }()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	var req Request
	if err := json.NewDecoder(conn).Decode(&req); err != nil {
		writeResp(conn, Response{OK: false, Error: "bad request: " + err.Error()})
		return
	}
	writeResp(conn, s.dispatch(ctx, req))
}

func writeResp(conn net.Conn, r Response) {
	r.Proto = ProtocolVersion
	_ = json.NewEncoder(conn).Encode(r)
}

func (s *Server) dispatch(ctx context.Context, req Request) Response {
	switch req.Op {
	case OpHealth:
		return Response{OK: true, Version: version.String()}
	case OpStatus:
		snap := s.snapshot()
		return Response{OK: true, Version: version.String(), Status: &snap}
	case OpMint:
		return s.handleMint(ctx)
	case OpKill:
		return s.handleKill(req.On)
	case OpShadow:
		return s.handleShadow(req.On)
	case OpGc:
		return s.handleGc()
	case OpShutdown:
		s.triggerShutdown()
		return Response{OK: true, Version: version.String()}
	default:
		return Response{OK: false, Error: "unknown op: " + string(req.Op)}
	}
}

// handleMint is the hot path. It waits up to mintReadyTimeout for a cold-started
// proxy to register, mints and records a session token, and pushes it over the
// seam. It is FAIL-OPEN: a proxy that never became ready, or a lost seam push,
// still yields a usable {Port, Token} so `ccs url` works — a dropped mint must
// not break the URL. It errors only when no proxy port is known at all.
func (s *Server) handleMint(ctx context.Context) Response {
	s.awaitProxyReady(ctx)

	s.mu.Lock()
	port := s.proxyPort
	mcpPort := s.mcpPort
	s.mu.Unlock()
	if port == 0 {
		return Response{OK: false, Error: "proxy data plane is not ready"}
	}

	token, err := Mint()
	if err != nil {
		return Response{OK: false, Error: err.Error()}
	}
	s.mu.Lock()
	s.tokens[token] = struct{}{}
	s.mu.Unlock()

	if err := s.seam.SendMint(string(token), s.relayConfig); err != nil {
		// Fail-open: the token is recorded and will be re-pushed on the next proxy
		// respawn; the URL must still be usable now.
		s.log.Printf("mint: push to proxy failed (token recorded, re-pushed on respawn): %v", err)
	}
	return Response{OK: true, Port: port, MCPPort: mcpPort, Token: string(token)}
}

// awaitProxyReady blocks until the proxy registers, the wait times out, or ctx
// is cancelled — so the first mint after a cold start does not race the child's
// bring-up.
func (s *Server) awaitProxyReady(ctx context.Context) {
	timeout := s.mintTimeout
	if timeout <= 0 {
		timeout = mintReadyTimeout
	}
	select {
	case <-s.proxyReady:
	case <-ctx.Done():
	case <-time.After(timeout):
	}
}

// handleKill records the kill toggle as the daemon's own state (the single
// source of truth — it is exactly what the proxy is now running), pushes it over
// the seam (fail-open), and refreshes status.json so both `ccs status` and `ccs
// kill status` reflect it immediately.
func (s *Server) handleKill(on bool) Response {
	s.mu.Lock()
	s.kill = on
	s.mu.Unlock()
	if err := s.seam.SendKill(on); err != nil {
		s.log.Printf("kill: push to proxy failed: %v", err)
	}
	s.publishStatus()
	return Response{OK: true, Kill: on}
}

// handleShadow records the shadow toggle as the daemon's own state, pushes it
// over the seam (fail-open), and refreshes status.json so the status views
// reflect it immediately.
func (s *Server) handleShadow(on bool) Response {
	s.mu.Lock()
	s.shadow = on
	s.mu.Unlock()
	if err := s.seam.SendShadow(on); err != nil {
		s.log.Printf("shadow: push to proxy failed: %v", err)
	}
	s.publishStatus()
	return Response{OK: true, Shadow: on}
}

// handleGc forwards a sweep request to the proxy over the seam, which computes
// the reachable set from every session's staged refs and evicts the rest. It is
// fail-open: with no proxy connected there is nothing to sweep, so the
// not-connected sentinel is reported as a benign error, not a daemon fault.
func (s *Server) handleGc() Response {
	if err := s.seam.SendGc(); err != nil {
		if errors.Is(err, proxyseam.ErrProxyNotConnected) {
			return Response{OK: false, Error: "proxy data plane is not connected; nothing to sweep"}
		}
		s.log.Printf("gc: push to proxy failed: %v", err)
		return Response{OK: false, Error: err.Error()}
	}
	return Response{OK: true}
}

// publishStatus mirrors the live snapshot to status.json so out-of-process
// readers (`ccs status`, `ccs kill status`) see the daemon's current state
// without querying the socket. A write failure is logged, not fatal — the
// in-memory snapshot OpStatus serves stays authoritative.
func (s *Server) publishStatus() {
	if err := WriteStatus(s.snapshot()); err != nil {
		s.log.Printf("write status: %v", err)
	}
}

// repushTokens re-mints every live session token to a freshly respawned proxy,
// so live sessions survive a proxy restart. Driven by the supervisor policy's
// Respawned reconcile. A failed push is logged, not fatal — the seam is
// fail-open.
func (s *Server) repushTokens() {
	s.mu.Lock()
	tokens := make([]Token, 0, len(s.tokens))
	for t := range s.tokens {
		tokens = append(tokens, t)
	}
	s.mu.Unlock()
	for _, t := range tokens {
		if err := s.seam.SendMint(string(t), s.relayConfig); err != nil {
			s.log.Printf("re-push token to respawned proxy: %v", err)
		}
	}
}

// snapshot assembles the daemon's current status view under the lock.
func (s *Server) snapshot() StatusSnapshot {
	s.mu.Lock()
	defer s.mu.Unlock()
	return StatusSnapshot{
		Proto:       ProtocolVersion,
		Version:     version.String(),
		GeneratedAt: time.Now().UTC(),
		ProxyPort:   s.proxyPort,
		ProxyMCPort: s.mcpPort,
		ProxyPID:    s.proxyPID,
		Sessions:    len(s.tokens),
		Kill:        s.kill,
		Shadow:      s.shadow,
	}
}
