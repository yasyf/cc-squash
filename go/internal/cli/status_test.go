package cli

import (
	"bytes"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/cc-squash/go/internal/paths"
)

func TestStatusJSONReadsPublishedSnapshot(t *testing.T) {
	want := control.StatusSnapshot{
		SchemaVersion: control.StatusSchemaVersion,
		Version:       "9.9.9",
		GeneratedAt:   time.Unix(1_700_000_000, 0).UTC(),
		ProxyPort:     50516,
		ProxyPID:      4242,
		Sessions:      3,
		Kill:          true,
	}
	if err := control.WriteStatus(want); err != nil {
		t.Fatalf("write status: %v", err)
	}

	cmd := newStatusCmd()
	var out bytes.Buffer
	cmd.SetOut(&out)
	cmd.SetErr(&out)
	cmd.SetArgs([]string{"--json"})
	if err := cmd.Execute(); err != nil {
		t.Fatalf("status --json: %v", err)
	}

	var got control.StatusSnapshot
	if err := json.Unmarshal(out.Bytes(), &got); err != nil {
		t.Fatalf("decode status JSON %q: %v", out.String(), err)
	}
	if got != want {
		t.Fatalf("status --json = %+v, want %+v", got, want)
	}
}

func TestStatusPlainTableRendersFields(t *testing.T) {
	if err := control.WriteStatus(control.StatusSnapshot{
		SchemaVersion: control.StatusSchemaVersion,
		Version:       "1.2.3", ProxyPort: 50600, ProxyPID: 7, Sessions: 2, Shadow: true,
	}); err != nil {
		t.Fatalf("write status: %v", err)
	}

	cmd := newStatusCmd()
	var out bytes.Buffer
	cmd.SetOut(&out)
	cmd.SetErr(&out)
	cmd.SetArgs(nil)
	if err := cmd.Execute(); err != nil {
		t.Fatalf("status: %v", err)
	}
	for _, want := range []string{"1.2.3", "50600", "SESSIONS", "SHADOW", "on"} {
		if !strings.Contains(out.String(), want) {
			t.Fatalf("status table missing %q:\n%s", want, out.String())
		}
	}
}

func TestStatusNotRunningIsNotAnError(t *testing.T) {
	unpublishStatus(t)
	cmd := newStatusCmd()
	var stdout, stderr bytes.Buffer
	cmd.SetOut(&stdout)
	cmd.SetErr(&stderr)
	cmd.SetArgs(nil)
	if err := cmd.Execute(); err != nil {
		t.Fatalf("status with no snapshot returned an error: %v", err)
	}
	if !strings.Contains(stderr.String(), "not running") {
		t.Fatalf("expected a not-running notice on stderr, got %q", stderr.String())
	}
	if stdout.Len() != 0 {
		t.Fatalf("expected no stdout when the daemon is down, got %q", stdout.String())
	}
}

// unpublishStatus removes the published snapshot so the not-running branch is
// reached. The state directory resolves through the passwd database, so HOME
// cannot relocate it and every status test shares the one real path.
func unpublishStatus(t *testing.T) {
	t.Helper()
	if err := os.Remove(paths.StatusPath()); err != nil && !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("unpublish status: %v", err)
	}
}
