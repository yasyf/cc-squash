package supervisor

import (
	"context"
	"io"
	"log"
	"net"
	"os"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/proxyseam"
)

func socketPair(t *testing.T) (parent, child net.Conn) {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		t.Fatalf("socketpair: %v", err)
	}
	ends := make([]net.Conn, 2)
	for i, fd := range fds {
		file := os.NewFile(uintptr(fd), "seam")
		conn, err := net.FileConn(file)
		_ = file.Close()
		if err != nil {
			t.Fatalf("file conn: %v", err)
		}
		ends[i] = conn
	}
	return ends[0], ends[1]
}

// liveSeam builds a real proxyseam.Server and returns the policy over it plus a
// function that spawns a fake child on its own handoff pair and registers it.
// repushed counts how many times the policy fired the re-push callback.
func liveSeam(t *testing.T) (policy *ProxyPolicy, connectChild func(version string) net.Conn, repushed *atomic.Int32) {
	t.Helper()
	seam := proxyseam.NewServer(log.New(io.Discard, "", 0))
	t.Cleanup(func() { _ = seam.Close() })

	var pushes atomic.Int32
	policy = NewProxyPolicy(seam, func() { pushes.Add(1) }, nil, log.New(io.Discard, "", 0))

	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)

	connectChild = func(version string) net.Conn {
		parent, child := socketPair(t)
		t.Cleanup(func() { _ = child.Close() })
		go seam.Serve(ctx, parent, policy.NoteRegistered)
		frame, err := proxyseam.Encode(proxyseam.Register{
			Type: proxyseam.MsgRegister, Protocol: proxyseam.ProtocolVersion,
			Port: 50515, MCPPort: 50516, Version: version, PID: os.Getpid(),
		})
		if err != nil {
			t.Fatalf("encode register: %v", err)
		}
		if _, err := child.Write(frame); err != nil {
			t.Fatalf("child register: %v", err)
		}
		return child
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
		t.Fatal("PeerAlive false with a live child channel")
	}

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
	if v := policy.Probe(); v.Reachable {
		t.Fatalf("probe reachable after ChildDied: %+v", v)
	}
	if _, err := policy.Kill(); err != ErrChildUnavailable {
		t.Fatalf("Kill after ChildDied = %v, want ErrChildUnavailable", err)
	}
}

func TestProxyPolicyKillNoChild(t *testing.T) {
	policy, _, _ := liveSeam(t)
	if _, err := policy.Kill(); err != ErrChildUnavailable {
		t.Fatalf("Kill with no spawned child = %v, want ErrChildUnavailable", err)
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

	if policy.WaitGone(context.Background(), 100*time.Millisecond) {
		t.Fatal("WaitGone reported gone while the child was live")
	}

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
// this frame; a steady-state Tick sends nothing, so the read times out.
func readShutdown(t *testing.T, child net.Conn, d time.Duration) bool {
	t.Helper()
	buf := make([]byte, 256)
	_ = child.SetReadDeadline(time.Now().Add(d))
	n, err := child.Read(buf)
	if err != nil {
		return false
	}
	msg, derr := proxyseam.Decode(buf[:n-1])
	if derr != nil {
		t.Fatalf("decode frame the child received: %v", derr)
	}
	if _, ok := msg.(proxyseam.Shutdown); !ok {
		t.Fatalf("child received %T, want Shutdown", msg)
	}
	return true
}

// TestSupervisorTickConvergesOnMatchedVersion drives a real Supervisor.Tick
// against a real registering proxy: a Tick whose MyVersion matches the proxy's
// registered version must NOT replace it, while a skewed MyVersion must.
func TestSupervisorTickConvergesOnMatchedVersion(t *testing.T) {
	cases := []struct {
		id              string
		registered      string
		myVersion       string
		wantReplaceTick bool
	}{
		{
			id:              "matched dev version is steady state",
			registered:      ProxyVersion(),
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

			spawn := &fakeSpawner{}
			sup := BuildSupervisor(spawn, policy, c.myVersion)
			sup.GoneWait = time.Second

			done := make(chan struct{})
			go func() { defer close(done); sup.Tick(context.Background()) }()

			got := readShutdown(t, child, 500*time.Millisecond)
			if got {
				_ = child.Close()
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
