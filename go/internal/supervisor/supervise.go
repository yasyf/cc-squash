// Package supervisor keeps the Rust ccs-proxy data-plane child alive at the
// control plane's own version. The generic state machine — revive a dead child
// under spawn backoff and a crash-loop breaker, spare an alive-but-wedged one,
// replace a version-skewed one — lives here; ProxyPolicy supplies every
// cc-squash judgement (what "reachable" means over the proxy-v1.sock seam, how the
// child is shut down / waited out / killed, what to re-establish after a
// respawn). SuperviseLoop drives the cadence.
package supervisor

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/yasyf/cc-squash/go/internal/version"
	"github.com/yasyf/daemonkit/proc"
)

// SuperviseInterval is the proxy supervision cadence: a crashed proxy is
// respawned (and live sessions re-minted) within ~10s rather than waiting on a
// human.
const SuperviseInterval = 10 * time.Second

// superviseIntervalEnv lets a test shrink the supervision cadence so a respawn
// is detected in milliseconds rather than a full production tick. Parsed as a
// Go duration (e.g. "50ms"); unset or unparseable falls back to
// SuperviseInterval. Production never sets it.
const superviseIntervalEnv = "CCS_SUPERVISE_INTERVAL"

const (
	// spawnBackoffBase / spawnBackoffCap bound the respawn backoff: consecutive
	// spawn failures double the wait 10s -> 20s -> ... capped at 10 minutes.
	spawnBackoffBase = 10 * time.Second
	spawnBackoffCap  = 10 * time.Minute

	// goneWait bounds the wait for a retiring proxy to release its seam after it
	// acks shutdown, before the supervisor reaps it.
	goneWait = 10 * time.Second

	// reviveBreaker is the crash-loop circuit-breaker threshold: after this many
	// consecutive deaths without the proxy ever settling at our version, the
	// supervisor stops reviving and calls Policy.Retreat.
	reviveBreaker = 3
)

// BuildSupervisor wires a Supervisor for the proxy child from the given
// spawn and policy, and Validates it — panicking on a misconfigured struct
// (a missing Required field) exactly when the daemon wires it, rather than
// nil-panicking deep inside a later revive or replace.
func BuildSupervisor(spawn Spawner, policy Policy, myVersion string) *Supervisor {
	sup := &Supervisor{
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
// supervision heartbeat. The cadence is SuperviseInterval unless
// CCS_SUPERVISE_INTERVAL shrinks it (test-only).
func SuperviseLoop(ctx context.Context, sup *Supervisor) {
	ticker := time.NewTicker(superviseInterval())
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

// superviseInterval resolves the supervision cadence: CCS_SUPERVISE_INTERVAL
// parsed as a Go duration when set and valid, else SuperviseInterval.
func superviseInterval() time.Duration {
	if d, err := time.ParseDuration(os.Getenv(superviseIntervalEnv)); err == nil && d > 0 {
		return d
	}
	return SuperviseInterval
}

// ProxyVersion is the version the daemon supervises the ccs-proxy child at — the
// version a same-repo proxy is expected to register, so the supervisor only
// reads a GENUINELY skewed (on-disk-upgraded) proxy as needing a replace. It is
// deliberately distinct from version.String() (the daemon's own display version,
// which also drives the daemon-vs-daemon peer-evict check): the proxy's version
// is minted by Cargo, not by the Go -ldflags, so the two are compared across a
// toolchain boundary and must be reconciled to a common shape.
//
// Reconciliation: the release tag stamps both binaries. The Go build carries
// the v-prefixed tag while the Rust build carries its normalized form, so the
// semver field is taken (first whitespace-delimited token, dropping any commit
// suffix) and a leading "v" is stripped. Unstamped builds both report "dev".
func ProxyVersion() string {
	if version.Version == "dev" {
		return "dev"
	}
	return normalizeProxyVersion(version.String())
}

// normalizeProxyVersion reconciles the daemon's display version into the version
// the ccs-proxy reports: the first whitespace-delimited field is taken and a
// leading "v" is stripped.
func normalizeProxyVersion(displayVersion string) string {
	return strings.TrimPrefix(strings.Fields(displayVersion)[0], "v")
}
