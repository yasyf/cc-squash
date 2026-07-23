package control

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"github.com/yasyf/cc-squash/go/internal/config"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/cc-squash/go/internal/supervisor"
	"github.com/yasyf/cc-squash/go/internal/version"
	dkdaemon "github.com/yasyf/daemonkit/daemon"
	"github.com/yasyf/daemonkit/daemonrole"
	"github.com/yasyf/daemonkit/drain"
	"github.com/yasyf/daemonkit/proc"
	dksupervise "github.com/yasyf/daemonkit/supervise"
	"github.com/yasyf/daemonkit/wire"
)

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

// Server is the cc-squash product control plane. Daemonkit owns its listener,
// process runtime, transport, admission, process identities, and reaping.
type Server struct {
	socket    string
	proxySock string
	log       *log.Logger
	role      daemonrole.Classifier
	readiness wire.ReadinessBarrier

	seam    *proxyseam.Server
	sup     *supervisor.Supervisor
	policy  *supervisor.ProxyPolicy
	spawner *proxySpawner
	pool    *dksupervise.Pool
	reaper  *proc.Reaper

	// spawnProxy overrides the detached ccs-proxy launch in tests; production
	// delegates launch, process limits, and reaping to daemonkit.
	spawnProxy func() error

	// mintTimeout bounds OpMint's wait for a cold-started proxy to register; zero
	// means mintReadyTimeout. Tests shrink it to pin the fail-open path fast.
	mintTimeout time.Duration

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

// NewServer returns a control-plane daemon for one exact stable executable role.
func NewServer(role daemonrole.Classifier) (*Server, error) {
	if err := role.Validate(); err != nil {
		return nil, fmt.Errorf("validate cc-squash daemon role: %w", err)
	}
	return &Server{
		socket:     paths.SocketPath(),
		proxySock:  paths.ProxySocketPath(),
		log:        log.New(os.Stderr, "[cc-squash] ", log.LstdFlags),
		role:       role,
		readiness:  &runtimeReadiness{},
		proxyReady: make(chan struct{}),
		tokens:     map[Token]struct{}{},
	}, nil
}

// Run is the entry point for `ccs daemon`.
func (s *Server) Run(ctx context.Context) error {
	runtime, err := s.runtime()
	if err != nil {
		return err
	}
	err = runtime.Run(ctx)
	if ctx.Err() != nil && errors.Is(err, ctx.Err()) {
		return nil
	}
	return err
}

func (s *Server) runtime() (*dkdaemon.Runtime, error) {
	if err := paths.EnsureStateDir(); err != nil {
		return nil, err
	}
	if err := paths.EnsureLockDir(); err != nil {
		return nil, err
	}
	var generation [16]byte
	if _, err := rand.Read(generation[:]); err != nil {
		return nil, fmt.Errorf("generate process generation: %w", err)
	}
	processStore := &proc.FileStore{Path: paths.ProcessStorePath()}
	s.reaper = &proc.Reaper{
		Store:      processStore,
		Generation: hex.EncodeToString(generation[:]),
	}
	pool, err := dksupervise.NewPool(1, s.reaper)
	if err != nil {
		return nil, err
	}
	s.pool = pool
	wireServer := &wire.Server{WireBuild: WireBuild}
	workers := &runtimeWorkers{server: s, pool: pool}
	readiness := s.readiness
	var runtime *dkdaemon.Runtime
	runtime, err = wire.NewRuntime(wire.RuntimeConfig{
		Socket: s.socket, RuntimeBuild: version.String(), RuntimeProtocol: int(wire.ProtocolVersion),
		Wire:       wireServer,
		Classifier: s.role, ReservedProtectedSessions: 1,
		StopVerifier: wire.StopVerifier{
			Classifier: s.role, Role: StopControlRoleID,
			Store: &proc.FileStore{Path: paths.ServiceProcessStorePath()},
		},
		Observations: []wire.ObservationRoute{{
			Op: wire.Op(OpRuntimeHealth), MaxResponseBytes: 16 << 10, AvailableBeforeReady: true,
			Handler: func(ctx context.Context, request wire.ObservationRequest) (wire.ObservationResponse, error) {
				if request.Tenant != "" {
					return wire.ObservationResponse{}, errors.New("cc-squash control requests do not carry a tenant")
				}
				if err := decodeStrict(request.Payload, &EmptyRequest{}); err != nil {
					return wire.ObservationResponse{}, err
				}
				health, err := runtime.Health(ctx)
				if err != nil {
					return wire.ObservationResponse{}, err
				}
				state, err := runtimeStateFromDaemon(health.State)
				if err != nil {
					return wire.ObservationResponse{}, err
				}
				payload, err := json.Marshal(Response{OK: true, RuntimeHealth: &RuntimeHealth{
					RuntimeBuild: health.RuntimeBuild, RuntimeProtocol: health.RuntimeProtocol, PID: health.PID,
					ProcessGeneration: health.ProcessGeneration, Ready: readiness.Published(),
					State: state, Draining: health.Draining, Busy: health.Busy,
				}})
				if err != nil {
					return wire.ObservationResponse{}, fmt.Errorf("encode runtime health observation: %w", err)
				}
				return wire.ObservationResponse{Payload: payload}, nil
			},
		}},
		Readiness: readiness,
		Admission: &drain.Intake{}, Workers: workers,
		State: runtimeState{}, Resources: runtimeResources{server: s},
		Activate: func(activation dkdaemon.Activation) error {
			return s.activate(activation, workers)
		},
	})
	if err != nil {
		pool.Close()
		pool.Cancel()
		_ = pool.Wait(context.Background())
		return nil, err
	}
	s.registerHandlers(wireServer)
	return runtime, nil
}

func (s *Server) activate(activation dkdaemon.Activation, workers *runtimeWorkers) error {
	if err := s.pool.Recover(activation.Startup); err != nil {
		return err
	}
	if _, err := s.reaper.RecoverReapReceipts(
		activation.Startup,
		proc.RecoveryTask,
		func(context.Context, proc.ReapReceipt) error { return clearRetiredProxyState() },
	); err != nil {
		return fmt.Errorf("settle retired proxy receipts: %w", err)
	}
	cfg, err := config.Load()
	if err != nil {
		s.log.Printf("config: load failed, pushing engine defaults: %v", err)
		cfg = json.RawMessage("{}")
	}
	s.relayConfig = cfg
	seam, err := proxyseam.NewServer(s.log)
	if err != nil {
		return err
	}
	s.seam = seam
	workerCtx, cancel := context.WithCancel(activation.Lifetime)
	workers.setCancel(cancel)
	spawner := &proxySpawner{server: s, pool: s.pool}
	s.spawner = spawner
	s.policy = supervisor.NewProxyPolicy(seam, s.repushTokens, spawner.Stop, s.log)
	s.log.Printf("daemon %s activated; socket=%s", version.String(), s.socket)
	s.wg.Add(2)
	go func() {
		defer s.wg.Done()
		seam.Start(workerCtx, s.onRegister)
	}()
	go func() {
		defer s.wg.Done()
		s.bringUp(workerCtx)
	}()
	return nil
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

func (s *Server) registerHandlers(server *wire.Server) {
	server.RegisterConcurrent(wire.Op(OpStatus), func(_ context.Context, request wire.Request) (any, error) {
		if err := decodeBusinessRequest(request, &EmptyRequest{}); err != nil {
			return nil, err
		}
		snapshot := s.snapshot()
		return Response{OK: true, Status: &snapshot}, nil
	})
	server.RegisterConcurrent(wire.Op(OpMint), func(ctx context.Context, request wire.Request) (any, error) {
		if err := decodeBusinessRequest(request, &EmptyRequest{}); err != nil {
			return nil, err
		}
		return s.handleMint(ctx), nil
	})
	server.RegisterConcurrent(wire.Op(OpKill), func(_ context.Context, request wire.Request) (any, error) {
		var message ToggleRequest
		if err := decodeBusinessRequest(request, &message); err != nil {
			return nil, err
		}
		return s.handleKill(message.On), nil
	})
	server.RegisterConcurrent(wire.Op(OpShadow), func(_ context.Context, request wire.Request) (any, error) {
		var message ToggleRequest
		if err := decodeBusinessRequest(request, &message); err != nil {
			return nil, err
		}
		return s.handleShadow(message.On), nil
	})
	server.RegisterConcurrent(wire.Op(OpGc), func(_ context.Context, request wire.Request) (any, error) {
		if err := decodeBusinessRequest(request, &EmptyRequest{}); err != nil {
			return nil, err
		}
		return s.handleGc(), nil
	})
}

func decodeBusinessRequest(request wire.Request, target any) error {
	if request.Tenant != "" {
		return errors.New("cc-squash control requests do not carry a tenant")
	}
	return decodeStrict(request.Payload, target)
}

func runtimeStateFromDaemon(state dkdaemon.State) (RuntimeState, error) {
	switch state {
	case dkdaemon.StateHealthy:
		return RuntimeStateHealthy, nil
	case dkdaemon.StateDegraded:
		return RuntimeStateDegraded, nil
	case dkdaemon.StateFailed:
		return RuntimeStateFailed, nil
	default:
		return "", fmt.Errorf("daemon runtime state %q is not exact", state)
	}
}

type runtimeState struct{}

func (runtimeState) Close() error { return nil }

type runtimeReadiness struct {
	published atomic.Bool
}

func (*runtimeReadiness) BeforeReady(context.Context) error { return nil }

func (r *runtimeReadiness) AfterReady(err error) { r.published.Store(err == nil) }

func (r *runtimeReadiness) Published() bool { return r.published.Load() }

type runtimeResources struct {
	server *Server
}

func (r runtimeResources) Close() error {
	if r.server.seam != nil {
		return r.server.seam.Close()
	}
	return nil
}

type runtimeWorkers struct {
	server *Server
	pool   *dksupervise.Pool

	mu     sync.Mutex
	cancel context.CancelFunc
}

func (w *runtimeWorkers) setCancel(cancel context.CancelFunc) {
	w.mu.Lock()
	w.cancel = cancel
	w.mu.Unlock()
}

func (w *runtimeWorkers) Close() {
	w.pool.Close()
	w.server.shutdownProxy()
}

func (w *runtimeWorkers) Cancel() {
	w.mu.Lock()
	cancel := w.cancel
	w.mu.Unlock()
	if cancel != nil {
		cancel()
	}
	w.pool.Cancel()
}

func (w *runtimeWorkers) Wait(ctx context.Context) error {
	poolErr := w.pool.Wait(ctx)
	done := make(chan struct{})
	go func() {
		w.server.wg.Wait()
		close(done)
	}()
	select {
	case <-done:
		return poolErr
	case <-ctx.Done():
		<-done
		return errors.Join(poolErr, ctx.Err())
	}
}

// shutdownProxy gracefully stops the supervised proxy on an intentional daemon
// shutdown: it sends the seam shutdown frame and waits (bounded) for the child to
// step down, so `ccs stop` / SIGTERM takes the proxy down with the daemon.
//
// Worker intake is already closed, so supervision cannot admit a replacement
// while the proxy steps down. The seam remains live until resource teardown:
// an explicit shutdown frame takes the proxy down, while a bare seam drop after
// a daemon crash still leaves it serving so live `ccs url` tokens survive until
// the daemon respawns.
func (s *Server) shutdownProxy() {
	if s.policy == nil {
		return
	}
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
	if err := WritePort(reg.Port); err != nil {
		s.log.Printf("write port-file: %v", err)
	}
	s.publishStatus()
	s.readyOnce.Do(func() { close(s.proxyReady) })
}

type proxySpawner struct {
	server  *Server
	pool    *dksupervise.Pool
	timeout time.Duration

	mu      sync.Mutex
	process *dksupervise.Process
}

func (p *proxySpawner) EnsureRunning(ctx context.Context) error {
	if p.server.policy.Registered() {
		return nil
	}
	if p.server.spawnProxy != nil {
		if err := p.server.spawnProxy(); err != nil {
			return fmt.Errorf("spawn proxy test child: %w", err)
		}
		return p.awaitReady(ctx)
	}
	bin, err := ProxyBinaryPath()
	if err != nil {
		return err
	}
	logFile, err := os.OpenFile(paths.LogPath(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return fmt.Errorf("open proxy log: %w", err)
	}
	defer logFile.Close()
	process, err := p.pool.Start(ctx, dksupervise.ProcessSpec{
		RecoveryClass: proc.RecoveryTask,
		Path:          bin,
		Args: []string{
			"--socket", p.server.proxySock,
			"--port", strconv.Itoa(p.server.currentProxyPort()),
			"--refs-db", paths.RefsDbPath(),
		},
		Stdout: logFile, Stderr: logFile, ReadinessTimeout: p.Timeout(),
		Ready: func(readyCtx context.Context, _ proc.Record) error {
			return p.awaitReady(readyCtx)
		},
	})
	if err != nil {
		return err
	}
	p.mu.Lock()
	p.process = process
	p.mu.Unlock()
	return nil
}

func (p *proxySpawner) Stop(ctx context.Context) (int, error) {
	p.mu.Lock()
	process := p.process
	p.mu.Unlock()
	if process == nil {
		return 0, proc.ErrChildUnavailable
	}
	pid := process.Record().PID
	if err := process.Stop(ctx); err != nil {
		return 0, err
	}
	p.mu.Lock()
	if p.process == process {
		p.process = nil
	}
	p.mu.Unlock()
	return pid, nil
}

func (p *proxySpawner) Timeout() time.Duration {
	if p.timeout > 0 {
		return p.timeout
	}
	return proc.DefaultSpawnTimeout
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
			return fmt.Errorf("%w: waiting for proxy: %w", proc.ErrChildUnavailable, ctx.Err())
		case <-deadline.C:
			return fmt.Errorf("%w: proxy did not register within %s", proc.ErrChildUnavailable, p.Timeout())
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
