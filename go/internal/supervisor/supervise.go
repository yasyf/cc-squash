// Package supervisor keeps the Rust ccs-proxy data-plane child alive at the
// control plane's own version. The generic state machine — revive a dead child
// under spawn backoff and a crash-loop breaker, spare an alive-but-wedged one,
// replace a version-skewed one — is proc.Supervisor; ProxyPolicy supplies every
// cc-squash judgement (what "reachable" means over the proxy.sock seam, how the
// child is shut down / waited out / killed, what to re-establish after a
// respawn). proc owns no ticker of its own, so SuperviseLoop drives the cadence.
package supervisor

import (
	"context"
	"fmt"
	"time"

	"github.com/yasyf/fusekit/proc"
)

// SuperviseInterval is the proxy supervision cadence: a crashed proxy is
// respawned (and live sessions re-minted) within ~10s rather than waiting on a
// human.
const SuperviseInterval = 10 * time.Second

const (
	// spawnBackoffBase / spawnBackoffCap bound the respawn backoff: consecutive
	// spawn failures double the wait 10s -> 20s -> ... capped at 10 minutes.
	spawnBackoffBase = 10 * time.Second
	spawnBackoffCap  = 10 * time.Minute

	// goneWait bounds the wait for a retiring proxy to release its seam after it
	// acks shutdown, before proc reaps it.
	goneWait = 10 * time.Second

	// reviveBreaker is the crash-loop circuit-breaker threshold: after this many
	// consecutive deaths without the proxy ever settling at our version, the
	// supervisor stops reviving and calls Policy.Retreat.
	reviveBreaker = 3
)

// BuildSupervisor wires a proc.Supervisor for the proxy child from the given
// spawn and policy, and Validates it — panicking on a misconfigured struct
// (a missing Required field) exactly when the daemon wires it, rather than
// nil-panicking deep inside a later revive or replace.
func BuildSupervisor(spawn proc.Spawn, policy proc.Policy, myVersion string) *proc.Supervisor {
	sup := &proc.Supervisor{
		Spawn:         spawn,
		MyVersion:     myVersion,
		Policy:        policy,
		GoneWait:      goneWait,
		SpawnBackoff:  proc.Backoff{Base: spawnBackoffBase, Cap: spawnBackoffCap},
		ReviveBreaker: reviveBreaker,
	}
	if err := sup.Validate(); err != nil {
		panic(fmt.Sprintf("supervisor: proxy supervisor misconfigured: %v", err))
	}
	return sup
}

// SuperviseLoop drives sup.Tick on a fixed cadence until ctx is cancelled. proc
// owns no ticker — the consumer drives the loop — so this is the proxy's
// supervision heartbeat.
func SuperviseLoop(ctx context.Context, sup *proc.Supervisor) {
	ticker := time.NewTicker(SuperviseInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			sup.Tick(ctx)
		}
	}
}
