package control

import (
	"os"
	"testing"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

func TestPortFileRoundTrip(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	const port = 50515
	if err := WritePort(port); err != nil {
		t.Fatalf("write: %v", err)
	}
	got, err := ReadPort()
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if got != port {
		t.Fatalf("got %d, want %d", got, port)
	}
}

func TestWritePortPerm0600(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	if err := WritePort(8080); err != nil {
		t.Fatalf("write: %v", err)
	}
	info, err := os.Stat(paths.PortFilePath())
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if perm := info.Mode().Perm(); perm != 0o600 {
		t.Fatalf("got perm %o, want 600", perm)
	}
}
