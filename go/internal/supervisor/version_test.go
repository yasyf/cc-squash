package supervisor

import (
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"testing"
)

func TestNormalizeProxyVersion(t *testing.T) {
	cases := []struct {
		id, display, want string
	}{
		{"dev falls back to the Cargo baseline", "dev", proxyDevVersion},
		{"release v-prefix is stripped", "v0.1.0", "0.1.0"},
		{"bare semver is unchanged", "0.1.0", "0.1.0"},
		{"commit suffix is dropped", "v0.4.2 (abc1234)", "0.4.2"},
		{"prerelease round-trips", "v0.2.0-rc.1", "0.2.0-rc.1"},
	}
	for _, c := range cases {
		t.Run(c.id, func(t *testing.T) {
			if got := normalizeProxyVersion(c.display); got != c.want {
				t.Fatalf("normalizeProxyVersion(%q) = %q, want %q", c.display, got, c.want)
			}
		})
	}
}

// cargoWorkspaceVersion matches the [workspace.package] version line in
// crates/Cargo.toml.
var cargoWorkspaceVersion = regexp.MustCompile(`(?m)^version = "([^"]+)"`)

// TestProxyDevVersionMatchesCargo pins proxyDevVersion to the Rust workspace
// version a dev ccs-proxy reports (CARGO_PKG_VERSION). Both binaries default to
// it when unstamped, so a Cargo bump that is not mirrored here would silently
// reintroduce the version-skew replace loop on every dev build — this asserts
// the two stay in lockstep, turning that drift into a red test.
func TestProxyDevVersionMatchesCargo(t *testing.T) {
	data, err := os.ReadFile(cargoManifest(t))
	if err != nil {
		t.Fatalf("read Cargo.toml: %v", err)
	}
	m := cargoWorkspaceVersion.FindSubmatch(data)
	if m == nil {
		t.Fatal("no [workspace.package] version found in crates/Cargo.toml")
	}
	if got := string(m[1]); got != proxyDevVersion {
		t.Fatalf("proxyDevVersion = %q but crates/Cargo.toml workspace version = %q; bump them together", proxyDevVersion, got)
	}
}

// cargoManifest walks up from this test file to the repo root and returns the
// path to crates/Cargo.toml.
func cargoManifest(t *testing.T) string {
	t.Helper()
	_, file, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("runtime.Caller failed")
	}
	dir := filepath.Dir(file)
	for {
		manifest := filepath.Join(dir, "crates", "Cargo.toml")
		if _, err := os.Stat(manifest); err == nil {
			return manifest
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatalf("could not find crates/Cargo.toml above %s", file)
		}
		dir = parent
	}
}
