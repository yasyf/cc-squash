package control

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"syscall"
	"time"

	"github.com/yasyf/daemonkit/daemonrole"
	"github.com/yasyf/daemonkit/wire"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/version"
)

// DaemonRoleID is the exact service label shared by launch and peer trust.
const DaemonRoleID = "com.yasyf.cc-squash.daemon"

// StopControlRoleID is the exact receipt role authorized to settle the daemon.
const StopControlRoleID = "com.yasyf.cc-squash.stop-control"

// ErrDaemonUnavailable means the control socket could not be reached.
var ErrDaemonUnavailable = errors.New("cc-squash daemon not running")

// Client maintains one exact persistent business session. Failed business
// calls are never replayed.
type Client struct {
	socket       string
	wireBuild    string
	runtimeBuild string

	mu       sync.Mutex
	business *wire.Client
}

// NewClient returns a lazy persistent client for the current exact builds.
func NewClient() *Client {
	return newClient(paths.SocketPath(), WireBuild, version.String())
}

func newClient(socket, wireBuild, runtimeBuild string) *Client {
	return &Client{socket: socket, wireBuild: wireBuild, runtimeBuild: runtimeBuild}
}

// DaemonRole resolves the stable ccs executable alias shared by launch and
// protected-session classification.
func DaemonRole() (daemonrole.Classifier, error) {
	rolePath, err := exec.LookPath("ccs")
	if err != nil {
		return daemonrole.Classifier{}, fmt.Errorf("resolve ccs role alias: %w", err)
	}
	rolePath, err = filepath.Abs(rolePath)
	if err != nil {
		return daemonrole.Classifier{}, fmt.Errorf("resolve absolute ccs role alias: %w", err)
	}
	role := daemonrole.Classifier{RoleID: DaemonRoleID, RolePath: filepath.Clean(rolePath)}
	if err := role.Validate(); err != nil {
		return daemonrole.Classifier{}, err
	}
	return role, nil
}

// Close settles the persistent business session.
func (c *Client) Close() error {
	c.mu.Lock()
	business := c.business
	c.business = nil
	c.mu.Unlock()
	if business != nil {
		return business.Close()
	}
	return nil
}

// Current reports whether the exact current healthy non-draining runtime is
// serving the product business protocol.
func (c *Client) Current(ctx context.Context) bool {
	health, err := c.RuntimeHealth(ctx)
	return err == nil && c.current(health)
}

// RuntimeHealth returns the daemon's exact product-visible runtime state over
// the ordinary business session.
func (c *Client) RuntimeHealth(ctx context.Context) (RuntimeHealth, error) {
	response, err := c.call(ctx, OpRuntimeHealth, EmptyRequest{}, 2*time.Second)
	if err != nil {
		return RuntimeHealth{}, err
	}
	if !response.OK || response.RuntimeHealth == nil {
		return RuntimeHealth{}, errors.New("runtime.health returned no health snapshot")
	}
	if err := validateRuntimeHealth(*response.RuntimeHealth); err != nil {
		return RuntimeHealth{}, err
	}
	return *response.RuntimeHealth, nil
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

// WaitGone waits until no product business endpoint owns the socket.
func (c *Client) WaitGone(ctx context.Context, timeout time.Duration) error {
	waitCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		_, err := c.RuntimeHealth(waitCtx)
		if errors.Is(err, ErrDaemonUnavailable) {
			return nil
		}
		select {
		case <-waitCtx.Done():
			return fmt.Errorf("wait for daemon socket release (last error: %v): %w", err, waitCtx.Err())
		case <-ticker.C:
		}
	}
}

// WaitReady waits for the exact current healthy non-draining product runtime.
func (c *Client) WaitReady(ctx context.Context, timeout time.Duration) error {
	waitCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	var last RuntimeHealth
	var lastErr error
	for {
		last, lastErr = c.RuntimeHealth(waitCtx)
		if lastErr == nil && c.current(last) {
			return nil
		}
		select {
		case <-waitCtx.Done():
			return fmt.Errorf("wait for current daemon runtime (last=%+v err=%v): %w", last, lastErr, waitCtx.Err())
		case <-ticker.C:
		}
	}
}

func (c *Client) current(health RuntimeHealth) bool {
	return health.RuntimeBuild == c.runtimeBuild && health.RuntimeProtocol == int(wire.ProtocolVersion) &&
		health.Ready && health.State == RuntimeStateHealthy && !health.Draining
}

func validateRuntimeHealth(health RuntimeHealth) error {
	if health.RuntimeBuild == "" {
		return errors.New("runtime.health build is empty")
	}
	if health.RuntimeProtocol <= 0 {
		return errors.New("runtime.health protocol is invalid")
	}
	if health.PID <= 1 {
		return errors.New("runtime.health PID is invalid")
	}
	if health.ProcessGeneration == "" {
		return errors.New("runtime.health generation is empty")
	}
	switch health.State {
	case RuntimeStateHealthy, RuntimeStateDegraded, RuntimeStateFailed:
		return nil
	default:
		return fmt.Errorf("runtime.health state %q is invalid", health.State)
	}
}

func (c *Client) call(ctx context.Context, op Op, request any, timeout time.Duration) (Response, error) {
	payload, err := json.Marshal(request)
	if err != nil {
		return Response{}, fmt.Errorf("encode %s request: %w", op, err)
	}
	callCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	session, err := c.businessSession(callCtx)
	if err != nil {
		return Response{}, err
	}
	result, err := session.Call(callCtx, wire.Op(op), "", payload)
	if err != nil {
		c.retireBusiness(session, err)
		return Response{}, err
	}
	if result.Outcome != wire.Delivered {
		reason := result.Response.Reason
		if reason == "" {
			reason = result.Outcome.String()
		}
		return Response{}, fmt.Errorf("%s request rejected before dispatch: %s", op, reason)
	}
	if result.Response.Err != "" {
		return Response{}, errors.New(result.Response.Err)
	}
	var response Response
	if err := decodeStrict(result.Response.Payload, &response); err != nil {
		return Response{}, fmt.Errorf("decode %s response: %w", op, err)
	}
	return response, nil
}

func (c *Client) businessSession(ctx context.Context) (*wire.Client, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.business != nil {
		return c.business, nil
	}
	session, err := wire.NewClient(ctx, wire.ClientConfig{
		Dial: wire.UnixDialer(c.socket), WireBuild: c.wireBuild,
	})
	if err != nil {
		if unavailable(err) {
			return nil, ErrDaemonUnavailable
		}
		return nil, err
	}
	c.business = session
	return session, nil
}

func (c *Client) retireBusiness(session *wire.Client, cause error) {
	c.mu.Lock()
	if c.business == session {
		c.business = nil
	}
	c.mu.Unlock()
	_ = session.Abort(cause)
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

func unavailable(err error) bool {
	return errors.Is(err, os.ErrNotExist) || errors.Is(err, syscall.ENOENT) || errors.Is(err, syscall.ECONNREFUSED)
}
