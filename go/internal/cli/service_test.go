package cli

import (
	"bytes"
	"context"
	"errors"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/daemonkit"
)

type recordingLauncher struct {
	steps    []string
	health   daemonkit.Health
	healthOK bool
	closed   int
	deadline bool
}

func (l *recordingLauncher) Ensure(ctx context.Context) error {
	l.note(ctx, "ensure")
	return nil
}

func (l *recordingLauncher) Stop(ctx context.Context) error {
	l.note(ctx, "stop")
	return nil
}

func (l *recordingLauncher) Health(ctx context.Context) (daemonkit.Health, error) {
	l.note(ctx, "health")
	if !l.healthOK {
		return daemonkit.Health{}, control.ErrDaemonUnavailable
	}
	return l.health, nil
}

func (l *recordingLauncher) Close() error {
	l.closed++
	return nil
}

func (l *recordingLauncher) note(ctx context.Context, step string) {
	if _, ok := ctx.Deadline(); ok {
		l.deadline = true
	}
	l.steps = append(l.steps, step)
}

func useRecordingLauncher(t *testing.T) *recordingLauncher {
	t.Helper()
	launcher := &recordingLauncher{}
	previous := openDaemonLauncher
	openDaemonLauncher = func() (daemonLauncher, error) { return launcher, nil }
	t.Cleanup(func() { openDaemonLauncher = previous })
	return launcher
}

func TestEnsureDaemonCurrentConvergesUnderADeadline(t *testing.T) {
	launcher := useRecordingLauncher(t)
	if err := ensureDaemonCurrent(t.Context(), time.Second); err != nil {
		t.Fatalf("ensure current: %v", err)
	}
	if !slices.Equal(launcher.steps, []string{"ensure"}) {
		t.Fatalf("steps = %v, want [ensure]", launcher.steps)
	}
	if !launcher.deadline {
		t.Fatal("ensure ran on a context carrying no deadline")
	}
	if launcher.closed != 1 {
		t.Fatalf("closed = %d, want 1", launcher.closed)
	}
}

func TestRemoveDaemonServiceStopsUnderADeadline(t *testing.T) {
	launcher := useRecordingLauncher(t)
	if err := removeDaemonService(t.Context(), time.Second); err != nil {
		t.Fatalf("remove: %v", err)
	}
	if !slices.Equal(launcher.steps, []string{"stop"}) {
		t.Fatalf("steps = %v, want [stop]", launcher.steps)
	}
	if !launcher.deadline {
		t.Fatal("stop ran on a context carrying no deadline")
	}
	if launcher.closed != 1 {
		t.Fatalf("closed = %d, want 1", launcher.closed)
	}
}

func TestOpenFailureSurfacesRatherThanConverging(t *testing.T) {
	want := errors.New("no program")
	previous := openDaemonLauncher
	openDaemonLauncher = func() (daemonLauncher, error) { return nil, want }
	t.Cleanup(func() { openDaemonLauncher = previous })
	if err := ensureDaemonCurrent(t.Context(), time.Second); !errors.Is(err, want) {
		t.Fatalf("ensure current err = %v, want %v", err, want)
	}
}

func TestServiceStatusReportsTheReportedBuildAndPhase(t *testing.T) {
	launcher := useRecordingLauncher(t)
	launcher.healthOK = true
	launcher.health = daemonkit.Health{
		Phase:  daemonkit.PhaseReady,
		Detail: []byte(`{"runtime_build":"1.2.3"}`),
	}
	out := runServiceStatus(t)
	for _, want := range []string{"Daemon: running (1.2.3, ready)", "Socket:"} {
		if !strings.Contains(out, want) {
			t.Fatalf("service status output = %q, want %q", out, want)
		}
	}
	if !slices.Equal(launcher.steps, []string{"health"}) {
		t.Fatalf("steps = %v, want [health]", launcher.steps)
	}
}

func TestServiceStatusReportsAnUnreachableDaemon(t *testing.T) {
	useRecordingLauncher(t)
	if out := runServiceStatus(t); !strings.Contains(out, "Daemon: not responding") {
		t.Fatalf("service status output = %q", out)
	}
}

func TestPhaseNamesAreExact(t *testing.T) {
	for phase, want := range map[daemonkit.Phase]string{
		daemonkit.PhaseStarting: "starting",
		daemonkit.PhaseReady:    "ready",
		daemonkit.PhaseDraining: "draining",
		daemonkit.PhaseFailed:   "failed",
	} {
		if got := phaseName(phase); got != want {
			t.Fatalf("phaseName(%d) = %q, want %q", phase, got, want)
		}
	}
}

func runServiceStatus(t *testing.T) string {
	t.Helper()
	root := NewRootCmd()
	var out bytes.Buffer
	root.SetOut(&out)
	root.SetErr(&out)
	root.SetArgs([]string{"service", "status"})
	if err := root.ExecuteContext(t.Context()); err != nil {
		t.Fatalf("service status: %v", err)
	}
	return out.String()
}
