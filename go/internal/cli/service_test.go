package cli

import (
	"context"
	"slices"
	"testing"
	"time"

	"github.com/yasyf/cc-squash/go/internal/control"
	"github.com/yasyf/daemonkit/service"
	"github.com/yasyf/daemonkit/wire"
)

type recordingServiceController struct {
	desired [][]service.Agent
	stops   []service.StopRuntimeRequest
	steps   *[]string
}

func (c *recordingServiceController) Converge(_ context.Context, agents []service.Agent) error {
	if c.steps != nil {
		*c.steps = append(*c.steps, "converge")
	}
	c.desired = append(c.desired, append([]service.Agent(nil), agents...))
	return nil
}

func (c *recordingServiceController) StopRuntime(
	_ context.Context,
	spec service.StopRuntimeRequest,
) (service.StopReceipt, error) {
	if c.steps != nil {
		*c.steps = append(*c.steps, "stop")
	}
	c.stops = append(c.stops, spec)
	return service.StopReceipt{}, nil
}

func (*recordingServiceController) Close(context.Context) error { return nil }

type runtimeClientStub struct {
	current         bool
	health          control.RuntimeHealth
	healthAvailable bool
	waitReady       int
	waitGone        int
	steps           *[]string
}

func (c *runtimeClientStub) Current(context.Context) bool { return c.current }

func (c *runtimeClientStub) RuntimeHealth(context.Context) (control.RuntimeHealth, error) {
	if !c.healthAvailable {
		return control.RuntimeHealth{}, control.ErrDaemonUnavailable
	}
	return c.health, nil
}

func (c *runtimeClientStub) WaitReady(context.Context, time.Duration) error {
	if c.steps != nil {
		*c.steps = append(*c.steps, "ready")
	}
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
	program, err := service.CanonicalExecutable()
	if err != nil {
		t.Fatalf("canonical executable: %v", err)
	}

	if err := ensureDaemonCurrentWith(t.Context(), time.Second, client); err != nil {
		t.Fatalf("ensure current: %v", err)
	}
	if len(controller.desired) != 1 || len(controller.desired[0]) != 1 ||
		controller.desired[0][0].Label != control.DaemonRoleID ||
		controller.desired[0][0].Program != program {
		t.Fatalf("desired service state = %+v", controller.desired)
	}
	if _, err := controller.desired[0][0].Plist(); err != nil {
		t.Fatalf("render desired service: %v", err)
	}
	if got := controller.desired[0][0].Env["HOME"]; got != "/tmp/ccs-service-home" {
		t.Fatalf("service HOME = %q", got)
	}
	if client.waitReady != 1 {
		t.Fatalf("wait ready calls = %d, want 1", client.waitReady)
	}
}

func TestEnsureDaemonCurrentStopsOlderRuntimeBeforeConvergingSuccessor(t *testing.T) {
	t.Setenv("HOME", "/tmp/ccs-service-home")
	steps := []string{}
	controller := useRecordingServiceController(t)
	controller.steps = &steps
	client := &runtimeClientStub{
		healthAvailable: true,
		health: control.RuntimeHealth{
			RuntimeBuild: "0.0.1", RuntimeProtocol: int(wire.ProtocolVersion), PID: 42,
			ProcessGeneration: "older-runtime", Ready: true, State: control.RuntimeStateHealthy,
		},
		steps: &steps,
	}

	if err := ensureDaemonCurrentWith(t.Context(), time.Second, client); err != nil {
		t.Fatalf("ensure current: %v", err)
	}
	if !slices.Equal(steps, []string{"stop", "converge", "ready"}) {
		t.Fatalf("replacement steps = %v", steps)
	}
	if len(controller.stops) != 1 {
		t.Fatalf("stop specs = %+v", controller.stops)
	}
	spec := controller.stops[0]
	if spec.OperationID != "cc-squash.stop-runtime.v1:older-runtime" ||
		spec.ControlRole != control.StopControlRoleID || spec.ExpectedRuntimeBuild != "0.0.1" ||
		spec.RuntimeClientConfig.Client.Role != control.StopControlRoleID ||
		spec.RuntimeClientConfig.Client.WireBuild != control.WireBuild ||
		spec.RuntimeClientConfig.Client.Dial == nil || spec.RuntimeClientConfig.NoProgressTimeout != serviceCloseTimeout {
		t.Fatalf("stop spec = %+v", spec)
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
