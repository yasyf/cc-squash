package cli

import (
	"context"
	"testing"
	"time"

	"github.com/yasyf/daemonkit/service"
)

type recordingServiceController struct {
	desired [][]service.Agent
}

func (c *recordingServiceController) Converge(_ context.Context, agents []service.Agent) error {
	c.desired = append(c.desired, append([]service.Agent(nil), agents...))
	return nil
}

func (*recordingServiceController) Close(context.Context) error { return nil }

type runtimeClientStub struct {
	current   bool
	waitReady int
	waitGone  int
}

func (c *runtimeClientStub) Current(context.Context) bool { return c.current }

func (c *runtimeClientStub) WaitReady(context.Context, time.Duration) error {
	c.waitReady++
	return nil
}

func (c *runtimeClientStub) WaitGone(context.Context, time.Duration) error {
	c.waitGone++
	return nil
}

func useRecordingServiceController(t *testing.T) *recordingServiceController {
	t.Helper()
	controller := &recordingServiceController{}
	previous := openServiceController
	openServiceController = func(context.Context) (serviceController, error) { return controller, nil }
	t.Cleanup(func() { openServiceController = previous })
	return controller
}

func TestEnsureDaemonCurrentUsesBusinessHealthBeforeServiceState(t *testing.T) {
	client := &runtimeClientStub{current: true}
	previous := openServiceController
	openServiceController = func(context.Context) (serviceController, error) {
		t.Fatal("healthy runtime unexpectedly opened service controller")
		return nil, nil
	}
	t.Cleanup(func() { openServiceController = previous })

	if err := ensureDaemonCurrentWith(t.Context(), time.Second, client); err != nil {
		t.Fatalf("ensure current: %v", err)
	}
	if client.waitReady != 0 {
		t.Fatalf("wait ready calls = %d, want 0", client.waitReady)
	}
}

func TestEnsureDaemonCurrentConvergesServiceThenWaitsForBusinessHealth(t *testing.T) {
	t.Setenv("HOME", "/tmp/ccs-service-home")
	controller := useRecordingServiceController(t)
	client := &runtimeClientStub{}

	if err := ensureDaemonCurrentWith(t.Context(), time.Second, client); err != nil {
		t.Fatalf("ensure current: %v", err)
	}
	if len(controller.desired) != 1 || len(controller.desired[0]) != 1 ||
		controller.desired[0][0].Label != ccsAgent().Label {
		t.Fatalf("desired service state = %+v", controller.desired)
	}
	if got := controller.desired[0][0].Env["HOME"]; got != "/tmp/ccs-service-home" {
		t.Fatalf("service HOME = %q", got)
	}
	if client.waitReady != 1 {
		t.Fatalf("wait ready calls = %d, want 1", client.waitReady)
	}
}

func TestInstallAndRemoveUseExactServiceStateBarriers(t *testing.T) {
	controller := useRecordingServiceController(t)
	client := &runtimeClientStub{}

	if err := installDaemonServiceWith(t.Context(), time.Second, client); err != nil {
		t.Fatalf("install: %v", err)
	}
	if err := removeDaemonServiceWith(t.Context(), time.Second, client); err != nil {
		t.Fatalf("remove: %v", err)
	}
	if len(controller.desired) != 2 || len(controller.desired[0]) != 1 || controller.desired[1] != nil {
		t.Fatalf("desired transitions = %+v", controller.desired)
	}
	if client.waitReady != 1 || client.waitGone != 1 {
		t.Fatalf("barriers: ready=%d gone=%d", client.waitReady, client.waitGone)
	}
}
