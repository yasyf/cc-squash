package control

import (
	"os"
	"path/filepath"
	"testing"
)

func TestCanonicalExecutableResolvesSymlink(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "ccs-proxy-target")
	if err := os.WriteFile(target, []byte("proxy"), 0o755); err != nil {
		t.Fatalf("write target: %v", err)
	}
	link := filepath.Join(dir, "ccs-proxy")
	if err := os.Symlink(target, link); err != nil {
		t.Fatalf("symlink: %v", err)
	}
	got, err := canonicalExecutable(link)
	if err != nil {
		t.Fatalf("canonical executable: %v", err)
	}
	want, err := filepath.EvalSymlinks(target)
	if err != nil {
		t.Fatalf("canonical target: %v", err)
	}
	if got != want {
		t.Fatalf("canonical executable = %q, want %q", got, want)
	}
}
