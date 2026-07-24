package control

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/yasyf/cc-squash/go/internal/config"
	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/cc-squash/go/internal/supervisor"
	"github.com/yasyf/cc-squash/go/internal/version"
	dkdaemon "github.com/yasyf/daemonkit/daemon"
	"github.com/yasyf/daemonkit/proc"
	"github.com/yasyf/daemonkit/trust"
	"github.com/yasyf/daemonkit/wire"
	"github.com/yasyf/daemonkit/worker"
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

const (
	releaseTeamID            = "SXKCTF23Q2"
	releaseSigningIdentifier = "ccs"
	shutdownTimeout          = 30 * time.Second
	productShutdownTimeout   = 10 * time.Second
	productResultTimeout     = time.Second
)

// Server is the cc-squash product control plane. Daemonkit owns its listener,
// process runtime, transport, admission, process identities, and reaping.
type Server struct {
	socket    string
	proxySock string
	log       *log.Logger

	seam             *proxyseam.Server
	sup              *supervisor.Supervisor
	policy           *supervisor.ProxyPolicy
	spawner          *proxySpawner
	children         *proc.Manager
	productCtx       context.Context
	productCancel    context.CancelFunc
	supervisorCancel context.CancelFunc
	supervisorDone   chan struct{}

	// spawnProxy overrides the detached ccs-proxy launch in tests; the override
	// must publish its exact process record before connecting to the seam.
	spawnProxy func(func(proc.Identity) error) error

	// mintTimeout bounds OpMint's wait for a cold-started proxy to register; zero
	// means mintReadyTimeout. Tests shrink it to pin the fail-open path fast.
	mintTimeout time.Duration
	// beforePublication is a test-only readiness barrier.
	beforePublication func(context.Context) error

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

type serverPublication = dkdaemon.PublicationSlot[*Server]

// NewServer returns the control-plane daemon composition.
func NewServer() (*Server, error) {
	return &Server{
		socket:     paths.SocketPath(),
		proxySock:  paths.ProxySocketPath(),
		log:        log.New(os.Stderr, "[cc-squash] ", log.LstdFlags),
		proxyReady: make(chan struct{}),
		tokens:     map[Token]struct{}{},
	}, nil
}

// Run is the entry point for `ccs daemon`.
func (s *Server) Run(ctx context.Context) error {
	runtime, publication, err := s.runtime()
	if err != nil {
		return err
	}
	activation, err := runtime.Begin(ctx)
	if err != nil {
		return err
	}
	settlement, err := activation.ClaimProductSettlement()
	if err != nil {
		if activation.Context().Err() != nil {
			return runtime.Wait(context.Background())
		}
		return errors.Join(err, shutdownRuntime(runtime))
	}
	productDone := make(chan error, 1)
	go func() {
		<-activation.Context().Done()
		closeCtx, cancel := context.WithTimeout(context.Background(), productShutdownTimeout)
		defer cancel()
		if err := s.closeProduct(closeCtx); err != nil {
			productDone <- err
			return
		}
		productDone <- settlement.Complete()
	}()
	fail := func(cause error) error {
		runtimeErr := shutdownRuntime(runtime)
		return errors.Join(cause, runtimeErr, productResult(runtimeErr, productDone))
	}
	if err := s.activate(activation); err != nil {
		return fail(err)
	}
	if s.beforePublication != nil {
		if err := s.beforePublication(activation.Context()); err != nil {
			return fail(err)
		}
	}
	staged, err := publication.Stage(activation, s)
	if err != nil {
		return fail(err)
	}
	if err := activation.CommitReady(staged); err != nil {
		return fail(err)
	}
	watchDone := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			shutdownCtx, cancel := context.WithTimeout(context.Background(), shutdownTimeout)
			defer cancel()
			_ = runtime.Shutdown(shutdownCtx)
		case <-watchDone:
		}
	}()
	waitErr := runtime.Wait(context.Background())
	close(watchDone)
	productErr := productResult(waitErr, productDone)
	if ctx.Err() != nil && errors.Is(waitErr, ctx.Err()) {
		waitErr = nil
	}
	return errors.Join(waitErr, productErr)
}

func productResult(runtimeErr error, done <-chan error) error {
	if errors.Is(runtimeErr, dkdaemon.ErrShutdownIncomplete) {
		select {
		case err := <-done:
			return err
		default:
			return nil
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), productResultTimeout)
	defer cancel()
	select {
	case err := <-done:
		return err
	case <-ctx.Done():
		return fmt.Errorf("cc-squash: collect product settlement: %w", ctx.Err())
	}
}

func shutdownRuntime(runtime *dkdaemon.Runtime) error {
	ctx, cancel := context.WithTimeout(context.Background(), shutdownTimeout)
	defer cancel()
	return errors.Join(runtime.Shutdown(ctx), runtime.Wait(ctx))
}

func (s *Server) runtime() (*dkdaemon.Runtime, *serverPublication, error) {
	if err := paths.EnsureStateDir(); err != nil {
		return nil, nil, err
	}
	if err := paths.EnsureLockDir(); err != nil {
		return nil, nil, err
	}
	generation, err := proc.ProcessGeneration()
	if err != nil {
		return nil, nil, err
	}
	workerReaper := &proc.Reaper{
		Store: &proc.FileStore{Path: paths.WorkerProcessStorePath()}, Generation: generation,
	}
	runtimeWorkers, err := worker.NewPool(worker.Config{
		Capacity: 1, QueueCapacity: 1, MaxTotalRun: 5 * time.Second,
		MaxStdinBytes: 1 << 20, MaxStdoutBytes: 1 << 20, MaxStderrBytes: 1 << 20,
	}, workerReaper)
	if err != nil {
		return nil, nil, err
	}
	children, err := proc.NewManager(1, &proc.Reaper{
		Store: &proc.FileStore{Path: paths.ChildProcessStorePath()}, Generation: generation,
	})
	if err != nil {
		return nil, nil, err
	}
	s.children = children
	policy, err := trust.NewTrustPolicy(trust.TrustPolicyConfig{
		ExpectedUID: os.Geteuid(),
		Roles: map[trust.PeerRole]trust.Requirement{
			StopControlRoleID: {TeamID: releaseTeamID, SigningIdentifier: releaseSigningIdentifier},
			LifecycleRoleID:   {TeamID: releaseTeamID, SigningIdentifier: releaseSigningIdentifier},
		},
		AllowUnprotected: true,
		StopRoles:        []trust.PeerRole{StopControlRoleID},
		ReceiptRoles:     []trust.PeerRole{LifecycleRoleID},
		ReadinessRoles:   []trust.PeerRole{LifecycleRoleID},
	})
	if err != nil {
		return nil, nil, err
	}
	wireServer := &wire.Server{WireBuild: WireBuild}
	var runtime *dkdaemon.Runtime
	runtime, err = wire.NewRuntime(wire.RuntimeConfig{
		Socket: s.socket, RuntimeBuild: version.String(), RuntimeProtocol: int(wire.ProtocolVersion),
		Wire: wireServer, TrustPolicy: policy,
		StopControlStore: &proc.FileStore{Path: paths.ServiceProcessStorePath()},
		Observations: []wire.ObservationRoute{{
			Op: wire.Op(OpRuntimeHealth), MaxResponseBytes: 16 << 10,
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
					ProcessGeneration: health.ProcessGeneration.String(), Ready: health.Ready,
					State: state, Draining: health.Draining, Busy: health.Busy,
				}})
				if err != nil {
					return wire.ObservationResponse{}, fmt.Errorf("encode runtime health observation: %w", err)
				}
				return wire.ObservationResponse{Payload: payload}, nil
			},
		}},
		Workers: runtimeWorkers, Children: children, ShutdownTimeout: shutdownTimeout,
	})
	if err != nil {
		return nil, nil, err
	}
	s.registerHandlers(wireServer)
	return runtime, dkdaemon.NewPublicationSlot[*Server](runtime), nil
}

func (s *Server) activate(activation dkdaemon.Activation) error {
	recovery, err := activation.RecoveryCapability(proc.RecoveryTaskID)
	if err != nil {
		return err
	}
	if len(recovery.Receipt().Settled()) != 0 {
		if err := clearRetiredProxyState(); err != nil {
			return fmt.Errorf("clear retired proxy state: %w", err)
		}
	}
	if err := recovery.Consume(); err != nil {
		return err
	}
	cfg, err := config.Load()
	if err != nil {
		s.log.Printf("config: load failed, pushing engine defaults: %v", err)
		cfg = json.RawMessage("{}")
	}
	s.relayConfig = cfg
	seam, err := proxyseam.NewServer(activation.Context(), s.log)
	if err != nil {
		return err
	}
	s.seam = seam
	productCtx, productCancel := context.WithCancel(context.Background())
	supervisorCtx, supervisorCancel := context.WithCancel(productCtx)
	s.productCtx = productCtx
	s.productCancel = productCancel
	s.supervisorCancel = supervisorCancel
	s.supervisorDone = make(chan struct{})
	spawner := &proxySpawner{server: s, children: s.children}
	s.spawner = spawner
	s.policy = supervisor.NewProxyPolicy(seam, s.repushTokens, spawner.Stop, s.log)
	s.log.Printf("daemon %s activated; socket=%s", version.String(), s.socket)
	s.wg.Add(2)
	go func() {
		defer s.wg.Done()
		seam.Start(productCtx, s.onRegister)
	}()
	go func() {
		defer s.wg.Done()
		defer close(s.supervisorDone)
		s.bringUp(supervisorCtx)
	}()
	return nil
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

func (s *Server) registerHandlers(server *wire.Server) {
	server.Register(wire.HandlerSpec{Op: wire.Op(OpStatus), Concurrent: true, Handler: func(_ context.Context, request wire.Request) (any, error) {
		if err := decodeBusinessRequest(request, &EmptyRequest{}); err != nil {
			return nil, err
		}
		snapshot := s.snapshot()
		return Response{OK: true, Status: &snapshot}, nil
	}})
	server.Register(wire.HandlerSpec{Op: wire.Op(OpMint), Concurrent: true, Handler: func(ctx context.Context, request wire.Request) (any, error) {
		if err := decodeBusinessRequest(request, &EmptyRequest{}); err != nil {
			return nil, err
		}
		return s.handleMint(ctx), nil
	}})
	server.Register(wire.HandlerSpec{Op: wire.Op(OpKill), Concurrent: true, Handler: func(_ context.Context, request wire.Request) (any, error) {
		var message ToggleRequest
		if err := decodeBusinessRequest(request, &message); err != nil {
			return nil, err
		}
		return s.handleKill(message.On), nil
	}})
	server.Register(wire.HandlerSpec{Op: wire.Op(OpShadow), Concurrent: true, Handler: func(_ context.Context, request wire.Request) (any, error) {
		var message ToggleRequest
		if err := decodeBusinessRequest(request, &message); err != nil {
			return nil, err
		}
		return s.handleShadow(message.On), nil
	}})
	server.Register(wire.HandlerSpec{Op: wire.Op(OpGc), Concurrent: true, Handler: func(_ context.Context, request wire.Request) (any, error) {
		if err := decodeBusinessRequest(request, &EmptyRequest{}); err != nil {
			return nil, err
		}
		return s.handleGc(), nil
	}})
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

// shutdownProxy makes one bounded graceful step-down request after supervision
// has stopped. Daemonkit remains the sole exact child kill/reap authority.
func (s *Server) shutdownProxy(ctx context.Context) {
	if s.policy == nil {
		return
	}
	graceCtx, cancel := context.WithTimeout(ctx, proxyShutdownGrace)
	defer cancel()
	if err := s.policy.Shutdown(graceCtx); err != nil {
		if errors.Is(err, proxyseam.ErrProxyNotConnected) {
			return // no proxy connected; nothing to step down
		}
		s.log.Printf("shutdown proxy: %v", err)
		return
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
	server   *Server
	children *proc.Manager
	timeout  time.Duration

	mu           sync.Mutex
	process      *proc.PreparedChild
	receipt      proc.ProcessReceipt
	outputCancel context.CancelFunc
}

func (p *proxySpawner) EnsureRunning(ctx context.Context) error {
	if p.server.policy.Registered() {
		return nil
	}
	if p.server.spawnProxy != nil {
		if err := p.server.spawnProxy(p.server.seam.ExpectProcess); err != nil {
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
	request, err := proc.NewSpawnRequest(proc.SpawnConfig{
		RecoveryID: proc.RecoveryTaskID,
		Executable: bin,
		Args: []string{
			"--socket", p.server.proxySock,
			"--port", strconv.Itoa(p.server.currentProxyPort()),
			"--refs-db", paths.RefsDbPath(),
		},
		Stdin: proc.StdioNull, Stdout: proc.StdioPipe, Stderr: proc.StdioPipe,
	})
	if err != nil {
		_ = logFile.Close()
		return err
	}
	process, receipt, err := p.children.Prepare(ctx, request)
	if err != nil {
		_ = logFile.Close()
		return err
	}
	stdout, err := process.TakeStdout()
	if err != nil {
		_ = logFile.Close()
		return errors.Join(err, stopPreparedChild(process))
	}
	stderr, err := process.TakeStderr()
	if err != nil {
		_ = stdout.Close()
		_ = logFile.Close()
		return errors.Join(err, stopPreparedChild(process))
	}
	expected := receipt.ProcessIdentity()
	expected.Executable = receipt.ExpectedExecutable()
	if err := p.server.seam.ExpectProcess(expected); err != nil {
		_ = stdout.Close()
		_ = stderr.Close()
		_ = logFile.Close()
		return errors.Join(err, stopPreparedChild(process))
	}
	outputCtx, outputCancel := context.WithCancel(p.server.productCtx)
	p.server.captureProxyOutput(outputCtx, stdout, stderr, logFile)
	if err := process.Start(ctx); err != nil {
		outputCancel()
		return errors.Join(err, stopPreparedChild(process))
	}
	p.mu.Lock()
	p.process = process
	p.receipt = receipt
	p.outputCancel = outputCancel
	p.mu.Unlock()
	if err := p.awaitReady(ctx); err != nil {
		outputCancel()
		stopErr := stopPreparedChild(process)
		p.clear(process)
		return errors.Join(err, stopErr)
	}
	return nil
}

func stopPreparedChild(process *proc.PreparedChild) error {
	ctx, cancel := context.WithTimeout(context.Background(), proxyShutdownGrace)
	defer cancel()
	return process.Stop(ctx)
}

func (p *proxySpawner) Stop(ctx context.Context) (int, error) {
	p.mu.Lock()
	process := p.process
	receipt := p.receipt
	outputCancel := p.outputCancel
	p.mu.Unlock()
	if process == nil {
		return 0, supervisor.ErrChildUnavailable
	}
	pid := receipt.ProcessIdentity().PID
	if outputCancel != nil {
		outputCancel()
	}
	if err := process.Stop(ctx); err != nil {
		return 0, err
	}
	p.clear(process)
	return pid, nil
}

func (p *proxySpawner) clear(process *proc.PreparedChild) {
	p.mu.Lock()
	if p.process == process {
		p.process = nil
		p.receipt = proc.ProcessReceipt{}
		p.outputCancel = nil
	}
	p.mu.Unlock()
}

func (s *Server) captureProxyOutput(ctx context.Context, stdout, stderr io.ReadCloser, logFile *os.File) {
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		defer logFile.Close()
		var copies sync.WaitGroup
		copies.Add(2)
		go func() {
			defer copies.Done()
			defer stdout.Close()
			_, _ = io.Copy(logFile, stdout)
		}()
		go func() {
			defer copies.Done()
			defer stderr.Close()
			_, _ = io.Copy(logFile, stderr)
		}()
		copied := make(chan struct{})
		go func() {
			copies.Wait()
			close(copied)
		}()
		select {
		case <-ctx.Done():
			_ = stdout.Close()
			_ = stderr.Close()
			<-copied
		case <-copied:
		}
	}()
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
