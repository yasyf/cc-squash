package supervisor

import (
	"context"
	"errors"
	"time"

	"github.com/yasyf/daemonkit/proc"
)

var (
	// ErrSkipSpawn means product policy intentionally declined one spawn attempt.
	ErrSkipSpawn = errors.New("supervisor: skip spawn")
	// ErrChildUnavailable means no exact product child remains under management.
	ErrChildUnavailable = errors.New("supervisor: child unavailable")
)

const defaultSpawnTimeout = 10 * time.Second

// ReconcileKind identifies a child lifecycle transition.
type ReconcileKind uint8

const (
	ChildDied ReconcileKind = iota
	Respawned
	ReplaceSucceeded
	ReplaceAborted
)

// Verdict is the policy's child reachability result.
type Verdict struct {
	Reachable bool
	Degraded  bool
	Version   string
}

// ReconcileEvent delivers one child lifecycle transition to the policy.
type ReconcileEvent struct {
	Kind ReconcileKind
}

// Policy supplies proxy-specific health, retirement, and recovery effects.
type Policy interface {
	Probe() Verdict
	PeerAlive() bool
	ReplaceSafe(ctx context.Context, force bool) string
	Retreat(ctx context.Context, reason string)
	Shutdown(ctx context.Context) error
	WaitGone(ctx context.Context, d time.Duration) bool
	Kill() (int, error)
	Reconcile(ctx context.Context, ev ReconcileEvent)
}

// Spawner starts the cc-squash proxy and waits until it is ready.
type Spawner interface {
	EnsureRunning(context.Context) error
	Timeout() time.Duration
}

// Supervisor keeps the detached proxy alive at MyVersion.
type Supervisor struct {
	Spawn         Spawner
	MyVersion     string
	Policy        Policy
	GoneWait      time.Duration
	HazardWindow  time.Duration
	SpawnBackoff  proc.Backoff
	ReviveBreaker int

	failures     int
	retryAt      time.Time
	reviveHazard int
	lastReviveAt time.Time
	sawUnhealthy bool
	spawnedSkew  string
}

const defaultHazardWindow = 30 * time.Minute

// Validate reports missing required wiring.
func (s *Supervisor) Validate() error {
	switch {
	case s.Policy == nil:
		return errors.New("supervisor: Policy is required")
	case s.MyVersion == "":
		return errors.New("supervisor: MyVersion is required")
	case s.Spawn == nil:
		return errors.New("supervisor: Spawn is required")
	}
	return nil
}

// Tick runs one proxy supervision pass.
func (s *Supervisor) Tick(ctx context.Context) {
	v := s.Policy.Probe()
	if !v.Reachable {
		s.revive(ctx)
		return
	}
	if v.Degraded {
		s.sawUnhealthy = false
		if s.isSkew(v.Version) {
			s.Replace(ctx, true)
		}
		return
	}
	s.sawUnhealthy = false
	if v.Version == "" {
		return
	}
	if !s.isSkew(v.Version) {
		s.resetSpawnBackoff()
		if v.Version == s.MyVersion {
			s.reviveHazard = 0
		}
		return
	}
	s.Replace(ctx, false)
}

func (s *Supervisor) isSkew(version string) bool {
	return version != "" && version != s.MyVersion && version != s.spawnedSkew
}

func (s *Supervisor) revive(ctx context.Context) {
	if !s.Policy.PeerAlive() && !s.sawUnhealthy {
		s.sawUnhealthy = true
		now := time.Now()
		if !s.lastReviveAt.IsZero() && now.Sub(s.lastReviveAt) > s.hazardWindow() {
			s.reviveHazard = 0
		}
		s.lastReviveAt = now
		s.reviveHazard++
		s.Policy.Reconcile(ctx, ReconcileEvent{Kind: ChildDied})
	}
	if s.ReviveBreaker > 0 && s.reviveHazard >= s.ReviveBreaker {
		s.Policy.Retreat(ctx, "proxy crash-looped without returning at this version")
		return
	}
	if time.Now().Before(s.retryAt) {
		return
	}
	if err := s.Spawn.EnsureRunning(ctx); err != nil {
		if errors.Is(err, ErrSkipSpawn) {
			return
		}
		s.noteSpawnFailure()
		if s.ReviveBreaker > 0 && s.failures >= s.ReviveBreaker {
			s.Policy.Retreat(ctx, "proxy will not spawn")
		}
		return
	}
	if !s.verifySpawned() {
		if s.ReviveBreaker > 0 && s.failures >= s.ReviveBreaker {
			s.Policy.Retreat(ctx, "proxy spawns but never becomes ready")
		}
		return
	}
	s.sawUnhealthy = false
	s.Policy.Reconcile(ctx, ReconcileEvent{Kind: Respawned})
}

// Replace retires a skewed proxy and starts a fresh one. It reports whether
// the policy deferred before taking ownership of replacement cleanup.
func (s *Supervisor) Replace(ctx context.Context, force bool) (deferred bool) {
	if reason := s.Policy.ReplaceSafe(ctx, force); reason != "" {
		return true
	}
	fired := false
	finalize := func(kind ReconcileKind) {
		if fired {
			return
		}
		fired = true
		s.Policy.Reconcile(ctx, ReconcileEvent{Kind: kind})
	}
	defer finalize(ReplaceAborted)

	if ctx.Err() != nil {
		return false
	}
	if err := s.Policy.Shutdown(ctx); err != nil {
		if !s.Policy.WaitGone(ctx, s.goneWait()) {
			if ctx.Err() != nil || !force || !s.reapWedged(ctx) {
				return false
			}
		}
	} else if !s.Policy.WaitGone(ctx, s.goneWait()) {
		if ctx.Err() != nil || !s.reapWedged(ctx) {
			return false
		}
	}
	if ctx.Err() != nil {
		return false
	}
	if err := s.Spawn.EnsureRunning(ctx); err != nil {
		if !errors.Is(err, ErrSkipSpawn) {
			s.noteSpawnFailure()
		}
		return false
	}
	if !s.verifySpawned() {
		return false
	}
	finalize(ReplaceSucceeded)
	return false
}

func (s *Supervisor) reapWedged(ctx context.Context) bool {
	_, err := s.Policy.Kill()
	if errors.Is(err, ErrChildUnavailable) {
		return true
	}
	if err != nil {
		return false
	}
	return s.Policy.WaitGone(ctx, s.goneWait())
}

func (s *Supervisor) verifySpawned() bool {
	v := s.Policy.Probe()
	if !v.Reachable || v.Degraded {
		s.noteSpawnFailure()
		return false
	}
	s.resetSpawnBackoff()
	s.noteSpawnedVersion(v.Version)
	return true
}

func (s *Supervisor) noteSpawnedVersion(version string) {
	switch {
	case version == "":
	case version == s.MyVersion:
		s.spawnedSkew = ""
	default:
		s.spawnedSkew = version
	}
}

func (s *Supervisor) noteSpawnFailure() {
	s.failures++
	s.retryAt = time.Now().Add(s.SpawnBackoff.After(s.failures))
}

func (s *Supervisor) resetSpawnBackoff() {
	s.failures = 0
	s.retryAt = time.Time{}
}

func (s *Supervisor) hazardWindow() time.Duration {
	if s.HazardWindow > 0 {
		return s.HazardWindow
	}
	return defaultHazardWindow
}

func (s *Supervisor) goneWait() time.Duration {
	if s.GoneWait > 0 {
		return s.GoneWait
	}
	if timeout := s.Spawn.Timeout(); timeout > 0 {
		return timeout
	}
	return defaultSpawnTimeout
}
