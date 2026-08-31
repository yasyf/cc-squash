package control

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/yasyf/cc-squash/go/internal/config"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/cc-squash/go/internal/supervisor"
	"github.com/yasyf/cc-squash/go/internal/version"
	"github.com/yasyf/daemonkit"
)

// mintReadyTimeout bounds how long OpMint waits for a cold-started proxy to
// register before it falls open and replies with whatever it knows.
const mintReadyTimeout = 3 * time.Second

// proxyStartupGrace bounds how long bringUp waits for the first proxy to
// register before starting the supervise loop anyway — long enough for a normal
// spawn+register (sub-second), so the first tick never races a healthy cold
// start into a spurious respawn.
const proxyStartupGrace = 5 * time.Second

// proxyShutdownGrace bounds how long an intentional daemon shutdown waits for the
// supervised proxy to step down after the seam shutdown frame, before the daemon
// returns and the seam Close drops the channel.
const proxyShutdownGrace = 3 * time.Second

// spawnSetupTimeout bounds the record write, probe, and exec verification of one
// proxy spawn — never the child's life.
const spawnSetupTimeout = 10 * time.Second

// Server is the cc-squash product control plane. daemonkit owns its listener,
// process runtime, transport, admission, process identities, and reaping.
type Server struct {
	log *log.Logger

	seam             *proxyseam.Server
	sup              *supervisor.Supervisor
	policy           *supervisor.ProxyPolicy
	spawner          *proxySpawner
	owner            daemonkit.Ctx
	productCtx       context.Context
	productCancel    context.CancelFunc
	supervisorCancel context.CancelFunc
	supervisorDone   chan struct{}

	// spawnProxy overrides the detached ccs-proxy launch in tests. It returns the
	// parent end of the child's handoff channel, which the seam then serves.
	spawnProxy func(context.Context) (proxyChild, error)

	// mintTimeout bounds OpMint's wait for a cold-started proxy to register; zero
	// means mintReadyTimeout. Tests shrink it to pin the fail-open path fast.
	mintTimeout time.Duration

	// relayConfig is the seam JSON parsed from config.toml once at daemon start
	// and pushed verbatim with every mint. Set in start before the supervisor
	// goroutine spawns, so the go-statement establishes the happens-before and
	// the mint/repush readers take no lock. A load error fails open to {} so a
	// bad config never blocks minting.
	relayConfig json.RawMessage

	// wg tracks every daemon goroutine (the startup bring-up, the supervise loop,
	// each seam session); Drain waits on it.
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

// NewServer returns the control-plane daemon composition.
func NewServer() (*Server, error) {
	return &Server{
		log:        log.New(os.Stderr, "[cc-squash] ", log.LstdFlags),
		proxyReady: make(chan struct{}),
		tokens:     map[Token]struct{}{},
	}, nil
}

// Run is the entry point for `ccs daemon`: daemonkit's whole lifecycle — flock,
// owner record, bind, product start, ready, serve, drain.
func (s *Server) Run(ctx context.Context) error {
	spec, err := Spec()
	if err != nil {
		return err
	}
	_, err = daemonkit.Serve(ctx, spec, s.start)
	s.log.Printf("daemon stopped")
	return err
}

func (s *Server) start(c daemonkit.Ctx) (daemonkit.Product, error) {
	if err := paths.EnsureStateDir(); err != nil {
		return nil, err
	}
	if len(c.Reclaimed) != 0 {
		if err := clearRetiredProxyState(); err != nil {
			return nil, fmt.Errorf("clear retired proxy state: %w", err)
		}
	}
	cfg, err := config.Load()
	if err != nil {
		s.log.Printf("config: load failed, pushing engine defaults: %v", err)
		cfg = json.RawMessage("{}")
	}
	s.relayConfig = cfg
	s.owner = c
	s.seam = proxyseam.NewServer(s.log)
	productCtx, productCancel := context.WithCancel(context.WithoutCancel(c.Context))
	supervisorCtx, supervisorCancel := context.WithCancel(productCtx)
	s.productCtx = productCtx
	s.productCancel = productCancel
	s.supervisorCancel = supervisorCancel
	s.supervisorDone = make(chan struct{})
	s.spawner = &proxySpawner{server: s}
	s.policy = supervisor.NewProxyPolicy(s.seam, s.repushTokens, s.spawner.Stop, s.log)
	socket, _ := SocketPath()
	s.log.Printf("daemon %s activated; socket=%s", version.String(), socket)
	c.Report(s.healthDetail())
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		defer close(s.supervisorDone)
		s.bringUp(supervisorCtx)
	}()
	return product{s}, nil
}

// healthDetail is cc-squash's half of daemonkit's health verb, which a launcher
// reads back to order this daemon against its own build.
func (s *Server) healthDetail() []byte {
	detail, _ := json.Marshal(HealthDetail{RuntimeBuild: version.String()})
	return detail
}

// product is the Server's daemonkit face: dispatch plus the two shutdown stages.
type product struct{ s *Server }

func (p product) Handle(ctx context.Context, req daemonkit.Request) (daemonkit.Reply, error) {
	return p.s.handle(ctx, req)
}

func (p product) Drain(b daemonkit.Budget) error {
	ctx, done := b.Context(context.Background())
	defer done()
	return p.s.closeProduct(ctx)
}

func (product) Close(daemonkit.Budget) error { return nil }

func (s *Server) handle(ctx context.Context, req daemonkit.Request) (daemonkit.Reply, error) {
	var response Response
	switch Op(req.Op) {
	case OpStatus:
		if err := decodeStrict(req.Body, &EmptyRequest{}); err != nil {
			return daemonkit.Reply{}, err
		}
		snapshot := s.snapshot()
		response = Response{OK: true, Status: &snapshot}
	case OpMint:
		if err := decodeStrict(req.Body, &EmptyRequest{}); err != nil {
			return daemonkit.Reply{}, err
		}
		response = s.handleMint(ctx)
	case OpKill:
		var message ToggleRequest
		if err := decodeStrict(req.Body, &message); err != nil {
			return daemonkit.Reply{}, err
		}
		response = s.handleKill(message.On)
	case OpShadow:
		var message ToggleRequest
		if err := decodeStrict(req.Body, &message); err != nil {
			return daemonkit.Reply{}, err
		}
		response = s.handleShadow(message.On)
	case OpGc:
		if err := decodeStrict(req.Body, &EmptyRequest{}); err != nil {
			return daemonkit.Reply{}, err
		}
		response = s.handleGc()
	default:
		return daemonkit.Reply{}, &daemonkit.ProductError{Code: "unknown_op", Message: "unknown op: " + req.Op}
	}
	body, err := json.Marshal(response)
	if err != nil {
		return daemonkit.Reply{}, fmt.Errorf("encode %s reply: %w", req.Op, err)
	}
	return daemonkit.Reply{Body: body}, nil
}

func (s *Server) closeProduct(ctx context.Context) error {
	if s.supervisorCancel != nil {
		s.supervisorCancel()
	}
	var supervisorErr error
	if s.supervisorDone != nil {
		select {
		case <-s.supervisorDone:
		case <-ctx.Done():
			supervisorErr = fmt.Errorf("cc-squash: join proxy supervisor: %w", ctx.Err())
		}
	}
	s.shutdownProxy(ctx)
	if s.productCancel != nil {
		s.productCancel()
	}
	var closeErr error
	if s.seam != nil {
		closeErr = s.seam.Close()
	}
	joined := make(chan struct{})
	go func() {
		s.wg.Wait()
		close(joined)
	}()
	select {
	case <-joined:
		return errors.Join(supervisorErr, closeErr)
	case <-ctx.Done():
		return errors.Join(supervisorErr, closeErr, fmt.Errorf("cc-squash: join product runtime: %w", ctx.Err()))
	}
}

func clearRetiredProxyState() error {
	var errs []error
	for _, path := range []string{paths.PortFilePath(), paths.StatusPath()} {
		if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
			errs = append(errs, err)
		}
	}
	return errors.Join(errs...)
}

// shutdownProxy makes one bounded graceful step-down request after supervision
// has stopped. daemonkit remains the sole exact child kill/reap authority.
func (s *Server) shutdownProxy(ctx context.Context) {
	if s.policy == nil {
		return
	}
	graceCtx, cancel := context.WithTimeout(ctx, proxyShutdownGrace)
	defer cancel()
	if err := s.policy.Shutdown(graceCtx); err != nil {
		if errors.Is(err, proxyseam.ErrProxyNotConnected) {
			return
		}
		s.log.Printf("shutdown proxy: %v", err)
		return
	}
}

// bringUp runs the deferred heavy startup off the ready path: it spawns the
// data-plane child, builds the supervisor, and drives the supervise loop until
// ctx is cancelled.
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
	if err := s.spawner.EnsureRunning(ctx); err != nil {
		s.log.Printf("spawn proxy: %v", err)
	}
	select {
	case <-s.proxyReady:
	case <-ctx.Done():
		return
	case <-time.After(proxyStartupGrace):
		s.log.Printf("proxy did not register within %s; starting supervision (revive will retry)", proxyStartupGrace)
	}
	s.sup = supervisor.BuildSupervisor(s.spawner, s.policy, supervisor.ProxyVersion())
	supervisor.SuperviseLoop(ctx, s.sup)
}

// onRegister captures a freshly registered proxy's identity, publishes its port
// (status mirror + port-file), and unblocks any OpMint waiting on the cold
// start. Runs on the seam's session goroutine.
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
	if err := WritePort(reg.Port); err != nil {
		s.log.Printf("write port-file: %v", err)
	}
	s.publishStatus()
	s.readyOnce.Do(func() { close(s.proxyReady) })
}

// proxyChild is the spawned data plane the seam speaks to: the handoff channel
// plus the pid and the settlement daemonkit owns.
type proxyChild interface {
	PID() int
	Conn() (net.Conn, error)
	Stop(context.Context) (daemonkit.Exit, error)
}

type proxySpawner struct {
	server  *Server
	timeout time.Duration

	mu    sync.Mutex
	child proxyChild
}

func (p *proxySpawner) EnsureRunning(ctx context.Context) error {
	if p.server.policy.Registered() {
		return nil
	}
	child, err := p.spawn(ctx)
	if err != nil {
		return err
	}
	conn, err := child.Conn()
	if err != nil {
		return errors.Join(err, p.stopChild(child))
	}
	p.mu.Lock()
	p.child = child
	p.mu.Unlock()
	p.server.wg.Add(1)
	go func() {
		defer p.server.wg.Done()
		p.server.seam.Serve(p.server.productCtx, conn, p.server.onRegister)
	}()
	if err := p.awaitReady(ctx); err != nil {
		stopErr := p.stopChild(child)
		p.clear(child)
		return errors.Join(err, stopErr)
	}
	return nil
}

func (p *proxySpawner) spawn(ctx context.Context) (proxyChild, error) {
	if p.server.spawnProxy != nil {
		return p.server.spawnProxy(ctx)
	}
	bin, err := ProxyBinaryPath()
	if err != nil {
		return nil, err
	}
	logFile, err := os.OpenFile(paths.LogPath(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open proxy log: %w", err)
	}
	spawnCtx, cancel := context.WithTimeout(ctx, spawnSetupTimeout)
	defer cancel()
	child, err := p.server.owner.Spawn(spawnCtx, daemonkit.Cmd{
		Path: bin,
		Args: []string{
			"--port", strconv.Itoa(p.server.currentProxyPort()),
			"--refs-db", paths.RefsDbPath(),
		},
		Session: true,
		Exec:    daemonkit.ServingSameUser(),
	}, daemonkit.ChannelHandoff, logFile)
	if err != nil {
		_ = logFile.Close()
		return nil, err
	}
	return child, nil
}

func (p *proxySpawner) stopChild(child proxyChild) error {
	ctx, cancel := context.WithTimeout(context.Background(), proxyShutdownGrace)
	defer cancel()
	_, err := child.Stop(ctx)
	return err
}

func (p *proxySpawner) Stop(ctx context.Context) (int, error) {
	p.mu.Lock()
	child := p.child
	p.mu.Unlock()
	if child == nil {
		return 0, supervisor.ErrChildUnavailable
	}
	pid := child.PID()
	if _, err := child.Stop(ctx); err != nil {
		return 0, err
	}
	p.clear(child)
	return pid, nil
}

func (p *proxySpawner) clear(child proxyChild) {
	p.mu.Lock()
	if p.child == child {
		p.child = nil
	}
	p.mu.Unlock()
}

func (p *proxySpawner) Timeout() time.Duration {
	if p.timeout > 0 {
		return p.timeout
	}
	return 10 * time.Second
}

func (p *proxySpawner) awaitReady(ctx context.Context) error {
	deadline := time.NewTimer(p.Timeout())
	defer deadline.Stop()
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		if p.server.policy.Registered() {
			return nil
		}
		select {
		case <-ctx.Done():
			// A child that registered between the last poll and the cancellation is
			// ready: reporting it unavailable would make the caller stop a live proxy
			// the seam is already serving, losing the graceful step-down.
			if p.server.policy.Registered() {
				return nil
			}
			return fmt.Errorf("%w: waiting for proxy: %w", supervisor.ErrChildUnavailable, ctx.Err())
		case <-deadline.C:
			return fmt.Errorf("%w: proxy did not register within %s", supervisor.ErrChildUnavailable, p.Timeout())
		case <-ticker.C:
		}
	}
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
// the seam (fail-open), and refreshes status-v1.json so both `ccs status` and `ccs
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
// over the seam (fail-open), and refreshes status-v1.json so the status views
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

// publishStatus mirrors the live snapshot to status-v1.json so out-of-process
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
		SchemaVersion: StatusSchemaVersion,
		Version:       version.String(),
		GeneratedAt:   time.Now().UTC(),
		ProxyPort:     s.proxyPort,
		ProxyMCPort:   s.mcpPort,
		ProxyPID:      s.proxyPID,
		Sessions:      len(s.tokens),
		Kill:          s.kill,
		Shadow:        s.shadow,
	}
}
