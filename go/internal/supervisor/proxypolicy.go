package supervisor

import (
	"context"
	"log"
	"sync"
	"time"

	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/daemonkit/proc"
)

// waitGonePoll is how often WaitGone re-checks the seam while a retiring proxy
// releases it.
const waitGonePoll = 50 * time.Millisecond

// ProxyPolicy implements Policy for the Rust ccs-proxy child. The generic
// revive/replace/breaker mechanism is Supervisor's; this policy supplies
// the cc-squash judgements and child-control effects, all routed through the
// proxy.sock seam (the data plane carries no separate health socket — the seam
// connection IS the liveness signal). The proxy is itself the data plane, so a
// crash-loop has no symlink fallback to retreat to: Retreat logs and the next
// Tick retries.
//
// Only the supervise goroutine (SuperviseLoop's Tick, or a test's direct Tick /
// Replace) drives these hooks, single-threaded; the captured registration is
// guarded by mu because the seam's accept goroutine writes it (NoteRegistered)
// while a Tick reads it.
type ProxyPolicy struct {
	seam *proxyseam.Server
	log  *log.Logger

	// repush re-mints every live session token to a freshly respawned proxy, so
	// live sessions survive a proxy restart. The daemon owns the token set; the
	// policy only triggers the re-push on the Respawned transition.
	repush func()
	stop   func(context.Context) (int, error)

	mu         sync.Mutex
	registered bool
	pid        int
	version    string
}

// NewProxyPolicy builds the proxy supervision policy over the seam. repush
// re-pushes the daemon's live session tokens after a respawn; diagnostics go to
// logger.
func NewProxyPolicy(
	seam *proxyseam.Server,
	repush func(),
	stop func(context.Context) (int, error),
	logger *log.Logger,
) *ProxyPolicy {
	return &ProxyPolicy{seam: seam, repush: repush, stop: stop, log: logger}
}

// NoteRegistered captures a proxy child's register frame — its os pid (for the
// peer-gated Kill) and its version — and marks the child registered. Called from
// the seam's accept goroutine when the proxy connects, so it takes the lock.
func (p *ProxyPolicy) NoteRegistered(reg proxyseam.Register) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.registered = true
	p.pid = reg.PID
	p.version = reg.Version
}

// Registered reports whether a proxy child has registered and its seam is still
// live — the Spawn.Available probe the supervisor consults before reviving, and
// the daemon's readiness gate for minting.
func (p *ProxyPolicy) Registered() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.registered && p.seam.Connected()
}

// Probe distills the seam into a Verdict: reachable iff a child registered
// and its seam connection is live, reporting the registered version. The data
// plane has no secondary readiness check, so a reachable proxy is never
// Degraded.
func (p *ProxyPolicy) Probe() Verdict {
	p.mu.Lock()
	defer p.mu.Unlock()
	if !p.registered || !p.seam.Connected() {
		return Verdict{Reachable: false}
	}
	return Verdict{Reachable: true, Version: p.version}
}

// PeerAlive reports whether the proxy's seam connection is still live — proc's
// dead(revive)-vs-wedged(spare) gate. The proxy exposes no separate peer
// channel, so a live seam connection is the only liveness signal.
func (p *ProxyPolicy) PeerAlive() bool {
	return p.seam.Connected()
}

// ReplaceSafe always clears: Layer 1 holds no claims a replace could race, so a
// version-skewed proxy may be retired immediately.
func (p *ProxyPolicy) ReplaceSafe(context.Context, bool) string {
	return ""
}

// Retreat is the crash-loop breaker action. The proxy IS the data plane, so
// there is no always-available fallback to retreat to (unlike cc-pool's
// fuse->symlink); the breaker reason is logged and the next Tick retries.
func (p *ProxyPolicy) Retreat(_ context.Context, reason string) {
	p.log.Printf("proxy supervision: %s; no data-plane fallback, retrying next tick", reason)
}

// Shutdown asks the proxy to step down over the seam.
func (p *ProxyPolicy) Shutdown(context.Context) error {
	return p.seam.SendShutdown()
}

// WaitGone reports whether the retiring proxy released its seam within d
// (connection dropped or registration cleared), polling until then or ctx
// cancellation.
func (p *ProxyPolicy) WaitGone(ctx context.Context, d time.Duration) bool {
	deadline := time.Now().Add(d)
	for {
		if !p.Registered() {
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		select {
		case <-ctx.Done():
			return false
		case <-time.After(waitGonePoll):
		}
	}
}

// Kill delegates exact process-group termination to daemonkit's managed
// process owner. The captured pid is observation only and never kill authority.
func (p *ProxyPolicy) Kill() (int, error) {
	if p.stop == nil {
		return 0, proc.ErrChildUnavailable
	}
	return p.stop(context.Background())
}

// Reconcile re-establishes desired state across a transition:
//   - Respawned: re-mint every live session token to the fresh proxy, so live
//     sessions survive a proxy restart.
//   - ChildDied: clear the captured pid/version so a stale identity is never
//     reused; the next register repopulates it.
func (p *ProxyPolicy) Reconcile(_ context.Context, ev ReconcileEvent) {
	switch ev.Kind {
	case Respawned, ReplaceSucceeded:
		p.log.Printf("proxy respawned; re-pushing live sessions")
		p.repush()
	case ChildDied:
		p.mu.Lock()
		p.registered = false
		p.pid = 0
		p.version = ""
		p.mu.Unlock()
		p.log.Printf("proxy unreachable; cleared captured identity")
	}
}
