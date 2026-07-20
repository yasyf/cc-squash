package supervisor

import (
	"context"
	"io"
	"log"
	"net"
	"os"
	"sync/atomic"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/proxyseam"
	"github.com/yasyf/daemonkit/proc"
)

// shortHome isolates the state dir under a short /tmp path: macOS caps a unix
// socket path at 104 bytes, and the default t.TempDir() overflows it once
// paths.ProxySocketPath() appends ~/.cc-squash/proxy-v1.sock.
func shortHome(t *testing.T) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "ccs-home")
	if err != nil {
		t.Fatalf("temp home: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	t.Setenv("HOME", dir)
}

// liveSeam binds a real proxyseam.Server, starts its accept loop wired to the
// policy's NoteRegistered, and returns the policy plus a function that connects a
// fake child and registers it. repushed counts how many times the policy fired
// the re-push callback.
func liveSeam(t *testing.T) (policy *ProxyPolicy, connectChild func(version string) net.Conn, repushed *atomic.Int32) {
	t.Helper()
	shortHome(t)
	seam, err := proxyseam.NewServer(log.New(io.Discard, "", 0))
	if err != nil {
		t.Fatalf("new seam: %v", err)
	}
	t.Cleanup(func() { _ = seam.Close() })

	var pushes atomic.Int32
	policy = NewProxyPolicy(seam, func() { pushes.Add(1) }, nil, log.New(io.Discard, "", 0))

	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	go seam.Start(ctx, policy.NoteRegistered)

	connectChild = func(version string) net.Conn {
		conn, derr := net.DialTimeout("unix", paths.ProxySocketPath(), time.Second)
		if derr != nil {
			t.Fatalf("child dial: %v", derr)
		}
		t.Cleanup(func() { _ = conn.Close() })
		frame, _ := proxyseam.Encode(proxyseam.Register{
			Type: proxyseam.MsgRegister, Protocol: proxyseam.ProtocolVersion,
			Port: 50515, MCPPort: 50516, Version: version, PID: os.Getpid(),
		})
		if _, werr := conn.Write(frame); werr != nil {
			t.Fatalf("child register: %v", werr)
		}
		return conn
	}
	return policy, connectChild, &pushes
}

// waitRegistered polls until the policy reports the child registered, or fails.
func waitRegistered(t *testing.T, policy *ProxyPolicy) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if policy.Registered() {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatal("policy never observed the child registering")
}

func TestProxyPolicyProbeAndPeerAlive(t *testing.T) {
	policy, connectChild, _ := liveSeam(t)

	// Before any child connects: unreachable, no peer.
	if v := policy.Probe(); v.Reachable {
		t.Fatalf("probe reachable before register: %+v", v)
	}
	if policy.PeerAlive() {
		t.Fatal("PeerAlive true before any child connected")
	}

	conn := connectChild("v9.9.9")
	waitRegistered(t, policy)

	v := policy.Probe()
	if !v.Reachable || v.Degraded || v.Version != "v9.9.9" {
		t.Fatalf("probe after register = %+v", v)
	}
	if !policy.PeerAlive() {
		t.Fatal("PeerAlive false with a live child connection")
	}

	// Drop the child: the seam connection clears, so Probe goes unreachable.
	_ = conn.Close()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) && policy.Probe().Reachable {
		time.Sleep(5 * time.Millisecond)
	}
	if policy.Probe().Reachable {
		t.Fatal("probe still reachable after the child dropped")
	}
}

func TestProxyPolicyReplaceSafeAlwaysClears(t *testing.T) {
	policy, _, _ := liveSeam(t)
	if reason := policy.ReplaceSafe(context.Background(), false); reason != "" {
		t.Fatalf("ReplaceSafe deferred at Layer 1: %q", reason)
	}
	if reason := policy.ReplaceSafe(context.Background(), true); reason != "" {
		t.Fatalf("ReplaceSafe(force) deferred: %q", reason)
	}
}

func TestProxyPolicyReconcileRespawnRepushes(t *testing.T) {
	policy, _, repushed := liveSeam(t)
	policy.Reconcile(context.Background(), ReconcileEvent{Kind: Respawned})
	if got := repushed.Load(); got != 1 {
		t.Fatalf("Respawned fired %d re-pushes, want 1", got)
	}
	policy.Reconcile(context.Background(), ReconcileEvent{Kind: ReplaceSucceeded})
	if got := repushed.Load(); got != 2 {
		t.Fatalf("ReplaceSucceeded fired %d total re-pushes, want 2", got)
	}
}

func TestProxyPolicyReconcileChildDiedClearsIdentity(t *testing.T) {
	policy, connectChild, _ := liveSeam(t)
	connectChild("v9.9.9")
	waitRegistered(t, policy)

	policy.Reconcile(context.Background(), ReconcileEvent{Kind: ChildDied})
	// Identity cleared: Probe reports unreachable and Kill has no pid to target.
	if v := policy.Probe(); v.Reachable {
		t.Fatalf("probe reachable after ChildDied: %+v", v)
	}
	if _, err := policy.Kill(); err != proc.ErrChildUnavailable {
		t.Fatalf("Kill after ChildDied = %v, want ErrChildUnavailable", err)
	}
}

func TestProxyPolicyKillNoPid(t *testing.T) {
	policy, _, _ := liveSeam(t)
	// No child ever registered: Kill refuses with ErrChildUnavailable so the supervisor
	// reads it as "nothing to kill, socket free".
	if _, err := policy.Kill(); err != proc.ErrChildUnavailable {
		t.Fatalf("Kill with no captured pid = %v, want ErrChildUnavailable", err)
	}
}

func TestProxyPolicyKillDelegatesToManagedProcessOwner(t *testing.T) {
	called := false
	policy := &ProxyPolicy{stop: func(context.Context) (int, error) {
		called = true
		return 4242, nil
	}}
	pid, err := policy.Kill()
	if err != nil || pid != 4242 || !called {
		t.Fatalf("Kill = pid %d, err %v, called %t", pid, err, called)
	}
}

func TestProxyPolicyWaitGone(t *testing.T) {
	policy, connectChild, _ := liveSeam(t)
	conn := connectChild("v9.9.9")
	waitRegistered(t, policy)

	// Still connected: WaitGone times out (the child has not left).
	if policy.WaitGone(context.Background(), 100*time.Millisecond) {
		t.Fatal("WaitGone reported gone while the child was live")
	}

	// Drop it: WaitGone observes the cleared seam within the window.
	_ = conn.Close()
	if !policy.WaitGone(context.Background(), 2*time.Second) {
		t.Fatal("WaitGone did not observe the dropped child")
	}
}

func TestProxyPolicyShutdownSendsOverSeam(t *testing.T) {
	policy, connectChild, _ := liveSeam(t)
	conn := connectChild("v9.9.9")
	waitRegistered(t, policy)

	if err := policy.Shutdown(context.Background()); err != nil {
		t.Fatalf("Shutdown: %v", err)
	}
	// The child receives a shutdown frame over the seam.
	buf := make([]byte, 256)
	_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
	n, err := conn.Read(buf)
	if err != nil {
		t.Fatalf("child read shutdown: %v", err)
	}
	msg, err := proxyseam.Decode(buf[:n-1])
	if err != nil {
		t.Fatalf("decode shutdown: %v", err)
	}
	if _, ok := msg.(proxyseam.Shutdown); !ok {
		t.Fatalf("child got %T, want Shutdown", msg)
	}
}

// readShutdown reports whether a Shutdown frame reached the child within d. A
// real Tick that decides to Replace calls Policy.Shutdown, which sends exactly
// this frame; a steady-state Tick sends nothing, so the read times out. This is
// the observable that tells a converged supervisor from one that re-replaces.
func readShutdown(t *testing.T, child net.Conn, d time.Duration) bool {
	t.Helper()
	buf := make([]byte, 256)
	_ = child.SetReadDeadline(time.Now().Add(d))
	switch n, err := child.Read(buf); {
	case err != nil:
		return false // timed out: no frame sent
	default:
		msg, derr := proxyseam.Decode(buf[:n-1])
		if derr != nil {
			t.Fatalf("decode frame the child received: %v", derr)
		}
		if _, ok := msg.(proxyseam.Shutdown); !ok {
			t.Fatalf("child received %T, want Shutdown", msg)
		}
		return true
	}
}

// TestSupervisorTickConvergesOnMatchedVersion drives a real Supervisor.Tick
// against a real registering proxy — the path the unit suite previously skipped
// (it only drove Tick against stub Probes that never matched a registered
// version against MyVersion). It pins the exact defect that flapped the proxy:
// a Tick whose MyVersion matches the proxy's registered version must NOT replace
// it, while a skewed MyVersion must. The match case uses ProxyVersion() and the
// proxy's real dev report, so it regression-guards the version-skew loop end to
// end.
func TestSupervisorTickConvergesOnMatchedVersion(t *testing.T) {
	cases := []struct {
		id              string
		registered      string // version the proxy registers with
		myVersion       string // version the supervisor runs at
		wantReplaceTick bool   // a Tick should send a Shutdown (replace) iff true
	}{
		{
			id:              "matched dev version is steady state",
			registered:      proxyDevVersion,
			myVersion:       ProxyVersion(),
			wantReplaceTick: false,
		},
		{
			id:              "genuinely skewed version replaces",
			registered:      "0.1.0",
			myVersion:       "0.2.0",
			wantReplaceTick: true,
		},
	}
	for _, c := range cases {
		t.Run(c.id, func(t *testing.T) {
			policy, connectChild, _ := liveSeam(t)
			child := connectChild(c.registered)
			waitRegistered(t, policy)

			// A no-op spawn keeps a replace from exec'ing a real binary.
			// The supervisor still drives the full Tick -> isSkew -> (Replace ->
			// Shutdown) decision over the real seam.
			spawn := &fakeSpawner{}
			// goneWait/spawn-timeout cap the replace legs to the test's timescale: on
			// the replace path the child drops its seam right after the Shutdown (the
			// proxy stepping down), so WaitGone returns promptly rather than running to
			// a production-length deadline.
			sup := BuildSupervisor(spawn, policy, c.myVersion)
			sup.GoneWait = time.Second

			done := make(chan struct{})
			go func() { defer close(done); sup.Tick(context.Background()) }()

			got := readShutdown(t, child, 500*time.Millisecond)
			if got {
				_ = child.Close() // the proxy steps down: release the seam so WaitGone returns
			}
			select {
			case <-done:
			case <-time.After(3 * time.Second):
				t.Fatal("Tick did not return; a replace leg blocked past its bound")
			}

			if got != c.wantReplaceTick {
				t.Fatalf("Tick at MyVersion=%q against registered=%q sent shutdown=%v, want replace=%v",
					c.myVersion, c.registered, got, c.wantReplaceTick)
			}
		})
	}
}
