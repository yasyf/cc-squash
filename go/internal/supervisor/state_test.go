package supervisor

import (
	"context"
	"testing"
	"time"

	"github.com/yasyf/fusekit/proc"
)

type statePolicy struct {
	verdict   Verdict
	peerAlive bool
	events    []ReconcileKind
	shutdowns int
}

func (p *statePolicy) Probe() Verdict { return p.verdict }
func (p *statePolicy) PeerAlive() bool {
	return p.peerAlive
}
func (*statePolicy) ReplaceSafe(context.Context, bool) string { return "" }
func (*statePolicy) Retreat(context.Context, string)          {}
func (p *statePolicy) Shutdown(context.Context) error {
	p.shutdowns++
	p.verdict = Verdict{}
	p.peerAlive = false
	return nil
}
func (*statePolicy) WaitGone(context.Context, time.Duration) bool { return true }
func (*statePolicy) Kill() (int, error)                           { return 0, nil }
func (p *statePolicy) Reconcile(_ context.Context, event ReconcileEvent) {
	p.events = append(p.events, event.Kind)
}

func stateSpawn(policy *statePolicy, version string, spawns *int) proc.Spawn {
	return proc.Spawn{
		Socket:    "/tmp/ccs-state-test.sock",
		Available: func() bool { return policy.verdict.Reachable },
		CanHost:   func() error { return nil },
		Override: func() error {
			(*spawns)++
			policy.verdict = Verdict{Reachable: true, Version: version}
			policy.peerAlive = true
			return nil
		},
	}
}

func TestSupervisorRevivesDeadProxy(t *testing.T) {
	policy := &statePolicy{}
	spawns := 0
	sup := BuildSupervisor(stateSpawn(policy, "v1", &spawns), policy, "v1")

	sup.Tick(t.Context())

	if spawns != 1 {
		t.Fatalf("spawns = %d, want 1", spawns)
	}
	want := []ReconcileKind{ChildDied, Respawned}
	if len(policy.events) != len(want) || policy.events[0] != want[0] || policy.events[1] != want[1] {
		t.Fatalf("events = %v, want %v", policy.events, want)
	}
}

func TestSupervisorReplacesSkewOnce(t *testing.T) {
	policy := &statePolicy{
		verdict:   Verdict{Reachable: true, Version: "v1"},
		peerAlive: true,
	}
	spawns := 0
	sup := BuildSupervisor(stateSpawn(policy, "v3", &spawns), policy, "v2")

	sup.Tick(t.Context())
	sup.Tick(t.Context())

	if policy.shutdowns != 1 {
		t.Fatalf("shutdowns = %d, want 1", policy.shutdowns)
	}
	if spawns != 1 {
		t.Fatalf("spawns = %d, want 1", spawns)
	}
	if len(policy.events) != 1 || policy.events[0] != ReplaceSucceeded {
		t.Fatalf("events = %v, want [%v]", policy.events, ReplaceSucceeded)
	}
}

func TestSupervisorReplaceCancellationFinalizes(t *testing.T) {
	policy := &statePolicy{verdict: Verdict{Reachable: true, Version: "v1"}, peerAlive: true}
	spawns := 0
	sup := BuildSupervisor(stateSpawn(policy, "v2", &spawns), policy, "v2")
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	sup.Replace(ctx, false)

	if len(policy.events) != 1 || policy.events[0] != ReplaceAborted {
		t.Fatalf("events = %v, want [%v]", policy.events, ReplaceAborted)
	}
	if policy.shutdowns != 0 || spawns != 0 {
		t.Fatalf("canceled replace mutated child: shutdowns=%d spawns=%d", policy.shutdowns, spawns)
	}
}
