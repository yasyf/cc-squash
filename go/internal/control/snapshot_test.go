package control

import (
	"os"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

func TestStatusRoundTrip(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	want := StatusSnapshot{
		SchemaVersion: StatusSchemaVersion,
		Version:       "1.2.3",
		GeneratedAt:   time.Unix(1_700_000_000, 0).UTC(),
		ProxyPort:     50516,
		ProxyPID:      4242,
		Sessions:      3,
		Kill:          true,
		Shadow:        true,
	}
	if err := WriteStatus(want); err != nil {
		t.Fatalf("write: %v", err)
	}
	got, err := ReadStatus()
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if got != want {
		t.Fatalf("round-trip mismatch: got %+v, want %+v", got, want)
	}
}

func TestReadStatusRejectsNonV1AndUnknownFields(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	if err := paths.EnsureStateDir(); err != nil {
		t.Fatalf("state dir: %v", err)
	}
	for _, data := range []string{
		`{"schema_version":0}`,
		`{"schema_version":1,"unknown":true}`,
	} {
		if err := os.WriteFile(paths.StatusPath(), []byte(data), 0o600); err != nil {
			t.Fatalf("write fixture: %v", err)
		}
		if _, err := ReadStatus(); err == nil {
			t.Fatalf("ReadStatus accepted %s", data)
		}
	}
}

func TestWriteStatusPerm0600(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	if err := WriteStatus(StatusSnapshot{SchemaVersion: StatusSchemaVersion}); err != nil {
		t.Fatalf("write: %v", err)
	}
	info, err := os.Stat(paths.StatusPath())
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if perm := info.Mode().Perm(); perm != 0o600 {
		t.Fatalf("got perm %o, want 600", perm)
	}
}
