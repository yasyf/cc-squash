package supervisor

import (
	"context"
	"testing"
	"time"

	"github.com/yasyf/fusekit/proc"
)

// stubPolicy is a minimal Policy whose Probe counts ticks, so a loop test
// can observe SuperviseLoop driving Tick.
type stubPolicy struct{ probes chan struct{} }

func (s stubPolicy) Probe() Verdict {
	select {
	case s.probes <- struct{}{}:
	default:
	}
	return Verdict{Reachable: true, Version: "v1"}
}
func (stubPolicy) PeerAlive() bool                              { return true }
func (stubPolicy) ReplaceSafe(context.Context, bool) string     { return "" }
func (stubPolicy) Retreat(context.Context, string)              {}
func (stubPolicy) Shutdown(context.Context) error               { return nil }
func (stubPolicy) WaitGone(context.Context, time.Duration) bool { return true }
func (stubPolicy) Kill() (int, error)                           { return 0, nil }
func (stubPolicy) Reconcile(context.Context, ReconcileEvent)    {}

func okSpawn() proc.Spawn {
	return proc.Spawn{
		Socket:    "/tmp/ccs-test.sock",
		Available: func() bool { return true },
		CanHost:   func() error { return nil },
	}
}

func TestBuildSupervisorValidates(t *testing.T) {
	sup := BuildSupervisor(okSpawn(), stubPolicy{}, "v1")
	if sup.MyVersion != "v1" {
		t.Fatalf("MyVersion = %q", sup.MyVersion)
	}
	if sup.ReviveBreaker != reviveBreaker {
		t.Fatalf("ReviveBreaker = %d, want %d", sup.ReviveBreaker, reviveBreaker)
	}
}

func TestBuildSupervisorPanicsOnMisconfig(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("BuildSupervisor did not panic on an empty MyVersion")
		}
	}()
	BuildSupervisor(okSpawn(), stubPolicy{}, "")
}

func TestSuperviseLoopStopsOnCancel(t *testing.T) {
	// A near-zero interval would still be 10s (SuperviseInterval is a const), so
	// this test asserts the loop EXITS promptly on cancel rather than counting
	// ticks — this test only pins prompt cancellation.
	sup := BuildSupervisor(okSpawn(), stubPolicy{probes: make(chan struct{}, 1)}, "v1")
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { defer close(done); SuperviseLoop(ctx, sup) }()
	cancel()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("SuperviseLoop did not return after ctx cancel")
	}
}
