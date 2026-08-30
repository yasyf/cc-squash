package control

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"time"

	"github.com/yasyf/daemonkit"
)

// ErrDaemonUnavailable means no daemon is listening on the control socket.
var ErrDaemonUnavailable = daemonkit.ErrAbsent

// closeTimeout bounds releasing a business session on Close.
const closeTimeout = 2 * time.Second

// Client reaches one cc-squash daemon: the business lane for product ops, the
// trust-gated control lane for health, and daemonkit's own convergence verbs.
type Client struct {
	daemon   daemonkit.Daemon
	client   *daemonkit.Client
	business *daemonkit.Business
}

// NewClient opens a lazy client for the daemon this build converges on. It
// performs no I/O beyond resolving the executable Ensure places.
func NewClient() (*Client, error) {
	spec, err := Spec()
	if err != nil {
		return nil, err
	}
	client, err := daemonkit.Open(spec)
	if err != nil {
		return nil, err
	}
	return &Client{daemon: spec, client: client, business: client.Business()}, nil
}

// Close releases the business session.
func (c *Client) Close() error {
	ctx, cancel := context.WithTimeout(context.Background(), closeTimeout)
	defer cancel()
	return c.business.Close(ctx)
}

// Health reports the pinned incumbent over the trust-gated control lane.
func (c *Client) Health(ctx context.Context) (daemonkit.Health, error) {
	control, err := c.client.Control(ctx)
	if err != nil {
		return daemonkit.Health{}, err
	}
	defer func() { _ = control.Close(ctx) }()
	return control.Health(ctx)
}

// Ensure makes the daemon be the exact build of this executable, ready and
// serving, cold-starting one when none runs.
func (c *Client) Ensure(ctx context.Context) error {
	if _, err := c.client.Ensure(ctx); err != nil {
		return fmt.Errorf("ensure daemon: %w", err)
	}
	return nil
}

// Stop leaves nothing serving at this daemon's label and no LaunchAgent behind
// it. It stops through a Daemon stating no Program, daemonkit's own contract:
// Stop renders no LaunchAgent and places nothing.
func (c *Client) Stop(ctx context.Context) error {
	stopping := c.daemon
	stopping.Program = daemonkit.Program{}
	client, err := daemonkit.Open(stopping)
	if err != nil {
		return err
	}
	if err := client.Stop(ctx); err != nil {
		return fmt.Errorf("stop daemon: %w", err)
	}
	return nil
}

// Status fetches the daemon's full status snapshot.
func (c *Client) Status(ctx context.Context) (Response, error) {
	return c.call(ctx, OpStatus, EmptyRequest{}, 5*time.Second)
}

// Mint asks the daemon to mint a fresh session token for the proxy.
func (c *Client) Mint(ctx context.Context) (Response, error) {
	return c.call(ctx, OpMint, EmptyRequest{}, 3*time.Second)
}

// Kill toggles the proxy kill switch.
func (c *Client) Kill(ctx context.Context, on bool) (Response, error) {
	return c.call(ctx, OpKill, ToggleRequest{On: on}, 2*time.Second)
}

// Shadow toggles the proxy's shadow mode.
func (c *Client) Shadow(ctx context.Context, on bool) (Response, error) {
	return c.call(ctx, OpShadow, ToggleRequest{On: on}, 2*time.Second)
}

// Gc asks the daemon to sweep the proxy's ref store to its reachable set.
func (c *Client) Gc(ctx context.Context) (Response, error) {
	return c.call(ctx, OpGc, EmptyRequest{}, 3*time.Second)
}

func (c *Client) call(ctx context.Context, op Op, request any, timeout time.Duration) (Response, error) {
	payload, err := json.Marshal(request)
	if err != nil {
		return Response{}, fmt.Errorf("encode %s request: %w", op, err)
	}
	callCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	reply, err := c.business.Call(callCtx, string(op), payload)
	if err != nil {
		return Response{}, err
	}
	var response Response
	if err := decodeStrict(reply.Body, &response); err != nil {
		return Response{}, fmt.Errorf("decode %s response: %w", op, err)
	}
	return response, nil
}

// ReportedBuild is the release the daemon published in its health detail, and
// whether it reported one at all.
func ReportedBuild(health daemonkit.Health) (string, bool) {
	if len(health.Detail) == 0 {
		return "", false
	}
	var reported HealthDetail
	if err := decodeStrict(health.Detail, &reported); err != nil {
		return "", false
	}
	return reported.RuntimeBuild, reported.RuntimeBuild != ""
}

func decodeStrict(payload []byte, target any) error {
	decoder := json.NewDecoder(bytes.NewReader(payload))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(target); err != nil {
		return err
	}
	if err := decoder.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		return errors.New("trailing JSON payload")
	}
	return nil
}
