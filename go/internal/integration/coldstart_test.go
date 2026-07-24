//go:build integration

// Package integration drives the two real cc-squash binaries — the Go `ccs`
// control plane and the Rust `ccs-proxy` data plane — together as separate
// processes, the way a user runs them. It is build-tagged `integration` so the
// default `go test ./...` CI gate never compiles or runs it (it shells out to
// `cargo`/`go build` and spawns real processes). Run it with:
//
//	cargo build -p ccs-proxy            # from crates/
//	cd go && go test -tags integration ./internal/integration/ -v
//
// The test builds both binaries into one temp dir (so ProxyBinaryPath's
// sibling-of-os.Executable resolution finds ccs-proxy next to ccs), points the
// spawned processes at an isolated short HOME, and proves: a cold `ccs url`
// prints a usable proxy URL backed by a listening ccs-proxy; a warm `ccs url`
// reuses the same daemon and port with a fresh token; and a SIGKILLed proxy is
// respawned by the supervisor on the SAME port, so previously-minted URLs keep
// resolving. No real upstream is contacted — only the control plane, the
// listening sockets, and port stability are exercised.
package integration

import (
	"bytes"
	"context"
	"encoding/json"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strconv"
	"syscall"
	"testing"
	"time"
)

// urlPattern matches the exact `ccs url` stdout: the proxy base URL plus the
// minted session token, nothing else. The capture groups are the port and token.
var urlPattern = regexp.MustCompile(`^http://127\.0\.0\.1:(\d+)/s/([^/\n]+)\n$`)

// superviseInterval is the shortened supervision cadence the spawned daemon runs
// at (via CCS_SUPERVISE_INTERVAL), so a respawn is detected in well under a
// second rather than the 10s production tick.
const superviseInterval = 100 * time.Millisecond

// statusSnapshot mirrors the fields of ~/.cc-squash/status-v1.json this test reads.
// It is decoded straight from `ccs status --json`.
type statusSnapshot struct {
	ProxyPort int `json:"proxy_port"`
	ProxyPID  int `json:"proxy_pid"`
	Sessions  int `json:"sessions"`
}

// harness owns the built binaries, the isolated HOME, and the env every spawned
// ccs process runs under.
type harness struct {
	t    *testing.T
	dir  string   // temp dir holding ./ccs and ./ccs-proxy
	home string   // short isolated HOME for ~/.cc-squash
	ccs  string   // path to the built ccs binary
	env  []string // env for every ccs invocation
}

func TestColdStartTwoProcess(t *testing.T) {
	h := newHarness(t)

	// Tear the daemon (and its proxy child) down no matter how the test exits, so
	// no process leaks and the user's real ~/.cc-squash is never touched.
	t.Cleanup(h.stop)

	port, token := h.coldStart()
	t.Logf("cold start: minted http://127.0.0.1:%d/s/%s", port, token)

	// Durably listening, not just transiently up: a perpetual version-skew Replace
	// loop re-binds the same port every supervise tick, so a retrying dial catches
	// a momentary listener and reports "up" while a real HTTP client sees most
	// connects fail. Single-shot, no-retry connects sampled over several ticks, plus
	// a stable proxy pid, distinguish a healthy supervised proxy from the flap.
	h.assertDurable(port)
	t.Logf("durable: proxy on port %d accepts every single-shot connect and holds one pid", port)

	warmPort, warmToken := h.warm(port, token)
	t.Logf("warm reuse: same port %d, fresh token %s", warmPort, warmToken)

	h.proxyRestart(port)
	t.Logf("proxy restart: respawned on the same port %d; minted URLs still resolve", port)

	// After a genuine SIGKILL respawn the proxy must settle, not keep flapping.
	// This is the assertion the old test lacked: "a different pid on the same port"
	// is satisfied every ~80ms by the replace loop, so it must also hold ONE stable
	// pid and accept every single-shot connect once respawned.
	h.assertDurable(port)
	t.Logf("durable after respawn: proxy settled on port %d, no replace flap", port)

	h.assertNoSkewLoop()
	t.Log("no version-skew replace loop in daemon.log")
}

// newHarness builds both real binaries into a temp dir and returns a harness
// pointed at an isolated short HOME.
func newHarness(t *testing.T) *harness {
	t.Helper()
	root := repoRoot(t)

	// A short /tmp HOME: macOS caps a unix socket path (sun_path) at 104 bytes, so
	// the default /var/folders t.TempDir() overflows once ~/.cc-squash/daemon.sock
	// is appended. MkdirTemp under /tmp keeps the leaves in range.
	home, err := os.MkdirTemp("/tmp", "ccs-it-home")
	if err != nil {
		t.Fatalf("temp HOME: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(home) })
	stateDir := filepath.Join(home, ".cc-squash")
	if err := os.MkdirAll(stateDir, 0o700); err != nil {
		t.Fatalf("state dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(stateDir, "config.toml"), []byte("schema_version = 1\n"), 0o600); err != nil {
		t.Fatalf("config: %v", err)
	}

	dir, err := os.MkdirTemp("/tmp", "ccs-it-bin")
	if err != nil {
		t.Fatalf("temp bin dir: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })

	ccsBin := filepath.Join(dir, "ccs")
	build := exec.Command("go", "build", "-o", ccsBin, "./cmd/ccs")
	build.Dir = filepath.Join(root, "go")
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("go build ccs: %v\n%s", err, out)
	}

	// Place ccs-proxy as a sibling of ccs so the daemon's ProxyBinaryPath
	// (sibling-of-os.Executable) resolves it without touching the user's PATH.
	cargoTarget := filepath.Join(dir, "cargo-target")
	cargo := exec.Command("cargo", "build", "-p", "ccs-proxy", "--target-dir", cargoTarget)
	cargo.Dir = filepath.Join(root, "crates")
	if out, err := cargo.CombinedOutput(); err != nil {
		t.Fatalf("cargo build ccs-proxy: %v\n%s", err, out)
	}
	copyExecutable(t, filepath.Join(cargoTarget, "debug", "ccs-proxy"), filepath.Join(dir, "ccs-proxy"))

	return &harness{
		t:    t,
		dir:  dir,
		home: home,
		ccs:  ccsBin,
		env: append(os.Environ(),
			"HOME="+home,
			"PATH="+dir+string(os.PathListSeparator)+os.Getenv("PATH"),
			"CCS_SUPERVISE_INTERVAL="+superviseInterval.String(),
		),
	}
}

// coldStart runs `ccs url` from a clean state, asserts the stdout is exactly the
// proxy URL, and confirms the real ccs-proxy is listening on the minted port.
func (h *harness) coldStart() (port int, token string) {
	h.t.Helper()
	out := h.run("url")
	port, token = parseURL(h.t, out)
	if !dialOK(port) {
		h.t.Fatalf("cold start: no listener on 127.0.0.1:%d after `ccs url`", port)
	}
	return port, token
}

// warm runs `ccs url` again and asserts it reuses the SAME proxy port (so the
// same daemon and proxy answered, no second cold start) while minting a DIFFERENT
// token.
func (h *harness) warm(coldPort int, coldToken string) (port int, token string) {
	h.t.Helper()
	start := time.Now()
	out := h.run("url")
	if elapsed := time.Since(start); elapsed > 3*time.Second {
		h.t.Fatalf("warm `ccs url` took %s; it should hit a live daemon, not cold-start", elapsed)
	}
	port, token = parseURL(h.t, out)
	if port != coldPort {
		h.t.Fatalf("warm port = %d, want the cold-start port %d (proxy was restarted or a second daemon answered)", port, coldPort)
	}
	if token == coldToken {
		h.t.Fatalf("warm token = %q, same as the cold token; each mint must be fresh", token)
	}
	if !dialOK(port) {
		h.t.Fatalf("warm: no listener on 127.0.0.1:%d", port)
	}
	return port, token
}

// proxyRestart SIGKILLs the live ccs-proxy child and waits (bounded, at the
// shortened supervise cadence) for the supervisor to respawn it on the SAME
// port, proving previously-minted URLs survive a proxy crash.
func (h *harness) proxyRestart(wantPort int) {
	h.t.Helper()
	before := h.status()
	if before.ProxyPort != wantPort {
		h.t.Fatalf("pre-kill status port = %d, want %d", before.ProxyPort, wantPort)
	}
	if before.ProxyPID == 0 {
		h.t.Fatal("pre-kill status has no proxy pid to kill")
	}

	if err := syscall.Kill(before.ProxyPID, syscall.SIGKILL); err != nil {
		h.t.Fatalf("SIGKILL proxy pid %d: %v", before.ProxyPID, err)
	}

	// Wait for the supervisor to bring a NEW proxy up (a different pid) on the
	// SAME port. Bounded generously relative to the ~100ms tick.
	deadline := time.Now().Add(15 * time.Second)
	for {
		snap := h.status()
		if snap.ProxyPID != 0 && snap.ProxyPID != before.ProxyPID && snap.ProxyPort == wantPort {
			break
		}
		if snap.ProxyPort != 0 && snap.ProxyPort != wantPort {
			h.t.Fatalf("respawned proxy bound port %d, want the prior port %d — outstanding URLs would break", snap.ProxyPort, wantPort)
		}
		if time.Now().After(deadline) {
			h.t.Fatalf("proxy was not respawned on port %d within the deadline (last status: pid=%d port=%d)", wantPort, snap.ProxyPID, snap.ProxyPort)
		}
		time.Sleep(50 * time.Millisecond)
	}

	// The respawned proxy is listening on the same port: a URL minted before the
	// crash still resolves to a live socket.
	if !dialTimeout(wantPort, 5*time.Second) {
		h.t.Fatalf("respawned proxy is not accepting on 127.0.0.1:%d", wantPort)
	}
}

// durableSamples / durableInterval bound the steady-state durability probe: how
// many single-shot connects to attempt and the gap between them. The window
// (~40 * 50ms = 2s) spans many shortened supervise ticks, so a proxy that
// flapped at the tick cadence would drop a large fraction of these no-retry
// connects and churn its pid.
const (
	durableSamples  = 40
	durableInterval = 50 * time.Millisecond
)

// assertDurable proves the proxy on port is durably listening: every single-shot,
// no-retry connect across the sampling window succeeds AND the proxy pid never
// changes. A perpetual replace loop fails both — it re-binds the same port only
// momentarily each tick (so most no-retry connects are refused) and mints a fresh
// pid every respawn. The deadline-retrying dialOK cannot see either; this can.
func (h *harness) assertDurable(port int) {
	h.t.Helper()
	pid := h.status().ProxyPID
	if pid == 0 {
		h.t.Fatal("assertDurable: status reports no proxy pid")
	}
	ok := 0
	for range durableSamples {
		if singleShotDial(port) {
			ok++
		}
		if got := h.status().ProxyPID; got != pid {
			h.t.Fatalf("proxy pid changed %d -> %d during the durability window — the proxy is flapping, not durably supervised", pid, got)
		}
		time.Sleep(durableInterval)
	}
	if ok != durableSamples {
		h.t.Fatalf("proxy on 127.0.0.1:%d accepted %d/%d single-shot connects; a durable listener accepts every one (a flapping replace loop drops most)", port, ok, durableSamples)
	}
}

// assertNoSkewLoop fails if the daemon ever logged a version-skew warning or a
// proxy-respawn re-push without a preceding SIGKILL — the operator-visible
// fingerprint of the perpetual Replace loop. The cold start and warm path do not
// kill the proxy, and the one deliberate SIGKILL in proxyRestart yields exactly
// one respawn, so a healthy run shows no skew warning at all.
func (h *harness) assertNoSkewLoop() {
	h.t.Helper()
	logBytes, err := os.ReadFile(filepath.Join(h.home, ".cc-squash", "daemon.log"))
	if err != nil {
		h.t.Fatalf("read daemon.log: %v", err)
	}
	if n := bytes.Count(logBytes, []byte("WARNING: proxy version")); n != 0 {
		h.t.Fatalf("daemon.log carries %d version-skew warnings — the supervised version does not match the proxy's; the replace loop is back:\n%s", n, logBytes)
	}
}

// run executes `ccs <args...>` under the isolated env, fails the test on a
// non-zero exit, and returns stdout.
func (h *harness) run(args ...string) string {
	h.t.Helper()
	var stdout, stderr bytes.Buffer
	cmd := exec.Command(h.ccs, args...)
	cmd.Env = h.env
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err := cmd.Run(); err != nil {
		daemonLog, _ := os.ReadFile(filepath.Join(h.home, ".cc-squash", "daemon.log"))
		h.t.Fatalf("ccs %v: %v\nstdout:\n%s\nstderr:\n%s\ndaemon.log:\n%s", args, err, stdout.String(), stderr.String(), daemonLog)
	}
	return stdout.String()
}

// status decodes `ccs status --json` into a snapshot.
func (h *harness) status() statusSnapshot {
	h.t.Helper()
	out := h.run("status", "--json")
	var snap statusSnapshot
	if err := json.Unmarshal([]byte(out), &snap); err != nil {
		h.t.Fatalf("decode `ccs status --json`: %v\noutput:\n%s", err, out)
	}
	return snap
}

// stop tears the daemon (and its proxy child) down, then sweeps any straggler
// proxy still bound to the temp dir's binary so a failed run never leaks a
// process.
func (h *harness) stop() {
	var stdout, stderr bytes.Buffer
	cmd := exec.Command(h.ccs, "stop")
	cmd.Env = h.env
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	_ = cmd.Run() // best-effort: a never-started daemon is fine

	// Sweep any ccs-proxy still running from THIS test's temp dir (e.g. a respawn
	// that outraced `stop`). pkill matches the full argv, so the temp path scopes
	// it to this test's children only.
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	_ = exec.CommandContext(ctx, "pkill", "-9", "-f", filepath.Join(h.dir, "ccs-proxy")).Run()
}

// repoRoot walks up from this test file to the cc-squash repo root — the dir
// holding both `crates/` and `go/`.
func repoRoot(t *testing.T) string {
	t.Helper()
	_, file, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("runtime.Caller failed")
	}
	dir := filepath.Dir(file)
	for {
		if isDir(filepath.Join(dir, "crates")) && isDir(filepath.Join(dir, "go")) {
			return dir
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatalf("could not find repo root (crates/ + go/) above %s", file)
		}
		dir = parent
	}
}

// parseURL asserts s is exactly the proxy URL line and returns its port and token.
func parseURL(t *testing.T, s string) (port int, token string) {
	t.Helper()
	m := urlPattern.FindStringSubmatch(s)
	if m == nil {
		t.Fatalf("`ccs url` stdout = %q, want exactly http://127.0.0.1:<port>/s/<token>\\n", s)
	}
	port, err := strconv.Atoi(m[1])
	if err != nil {
		t.Fatalf("parse port from %q: %v", s, err)
	}
	return port, m[2]
}

// dialOK reports whether a TCP connection to 127.0.0.1:port succeeds promptly.
func dialOK(port int) bool { return dialTimeout(port, time.Second) }

// singleShotDial is one no-retry TCP connect to 127.0.0.1:port — the probe a
// real HTTP client makes. Unlike dialTimeout it never retries, so a momentary
// listener during a replace flap does NOT mask a refused connect: this is what
// the assertDurable success-rate check is built on.
func singleShotDial(port int) bool {
	conn, err := net.DialTimeout("tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(port)), 200*time.Millisecond)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}

// dialTimeout reports whether a TCP connection to 127.0.0.1:port succeeds within
// d, retrying until then (a freshly bound listener may need a moment).
func dialTimeout(port int, d time.Duration) bool {
	deadline := time.Now().Add(d)
	addr := net.JoinHostPort("127.0.0.1", strconv.Itoa(port))
	for {
		conn, err := net.DialTimeout("tcp", addr, 200*time.Millisecond)
		if err == nil {
			_ = conn.Close()
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(20 * time.Millisecond)
	}
}

func copyExecutable(t *testing.T, src, dst string) {
	t.Helper()
	data, err := os.ReadFile(src)
	if err != nil {
		t.Fatalf("read %s: %v", src, err)
	}
	if err := os.WriteFile(dst, data, 0o755); err != nil {
		t.Fatalf("write %s: %v", dst, err)
	}
}

func isDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}
