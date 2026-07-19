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

	dkdaemon "github.com/yasyf/daemonkit/daemon"
	"github.com/yasyf/daemonkit/daemonrole"
	"github.com/yasyf/daemonkit/proc"
	"github.com/yasyf/daemonkit/wire"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/cc-squash/go/internal/version"
)

// DaemonRoleID is the exact service label shared by launch and peer trust.
const DaemonRoleID = "com.yasyf.cc-squash.daemon"

// ErrDaemonUnavailable means the control socket could not be reached.
var ErrDaemonUnavailable = errors.New("cc-squash daemon not running")

// Client maintains one exact persistent business session and one protected
// lifecycle session. Failed business calls are never replayed.
type Client struct {
	socket         string
	businessBuild  string
	lifecycleBuild string

	mu        sync.Mutex
	business  *wire.Client
	lifecycle *wire.LifecyclePeer
}

// NewClient returns a lazy persistent client for the current exact builds.
func NewClient() *Client {
	return newClient(paths.SocketPath(), BusinessBuild, version.String())
}

func newClient(socket, businessBuild, lifecycleBuild string) *Client {
	c := &Client{socket: socket, businessBuild: businessBuild, lifecycleBuild: lifecycleBuild}
	c.lifecycle = &wire.LifecyclePeer{Config: wire.ClientConfig{
		Dial: wire.UnixDialer(socket), Build: businessBuild, LifecycleBuild: lifecycleBuild,
	}}
	return c
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

// Close settles both persistent sessions.
func (c *Client) Close() error {
	c.mu.Lock()
	business := c.business
	c.business = nil
	lifecycle := c.lifecycle
	c.mu.Unlock()
	var businessErr error
	if business != nil {
		businessErr = business.Close()
	}
	return errors.Join(businessErr, lifecycle.Close())
}

// Available reports whether the exact lifecycle peer is reachable.
func (c *Client) Available(ctx context.Context) bool {
	_, err := c.Health(ctx)
	return err == nil
}

// Health returns daemonkit's exact release and lifecycle state.
func (c *Client) Health(ctx context.Context) (dkdaemon.Health, error) {
	health, err := c.lifecycle.Health(ctx)
	if errors.Is(err, dkdaemon.ErrNoPeer) || unavailable(err) {
		return dkdaemon.Health{}, ErrDaemonUnavailable
	}
	return health, err
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

// Shutdown requests daemonkit's ordered runtime shutdown.
func (c *Client) Shutdown(ctx context.Context) error {
	err := c.lifecycle.Shutdown(ctx)
	if errors.Is(err, dkdaemon.ErrNoPeer) || unavailable(err) {
		return ErrDaemonUnavailable
	}
	return err
}

// WaitGone waits until no lifecycle peer owns the socket.
func (c *Client) WaitGone(ctx context.Context, timeout time.Duration) error {
	waitCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		_, err := c.Health(waitCtx)
		if errors.Is(err, ErrDaemonUnavailable) {
			return nil
		}
		if err != nil {
			return err
		}
		select {
		case <-waitCtx.Done():
			return fmt.Errorf("wait for daemon socket release: %w", waitCtx.Err())
		case <-ticker.C:
		}
	}
}

// EnsureCurrent starts or upgrades the exact daemon release.
func (c *Client) EnsureCurrent(ctx context.Context, timeout time.Duration) error {
	if health, err := c.Health(ctx); err == nil &&
		health.Build == c.lifecycleBuild && health.Protocol == int(wire.ProtocolVersion) {
		return nil
	}
	if err := paths.EnsureStateDir(); err != nil {
		return err
	}
	if err := paths.EnsureLockDir(); err != nil {
		return err
	}
	role, err := DaemonRole()
	if err != nil {
		return err
	}
	spawn := proc.Spawn{
		Socket: c.socket, LogPath: paths.LogPath(), ExecPath: role.RolePath,
		Args: []string{"daemon"}, Timeout: timeout,
		Available: func() bool {
			probeCtx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
			defer cancel()
			health, err := c.lifecycle.Health(probeCtx)
			return err == nil && health.Build == c.lifecycleBuild && health.Protocol == int(wire.ProtocolVersion)
		},
		CanHost: func() error { return nil },
	}
	return dkdaemon.EnsureCurrent(ctx, dkdaemon.EnsureConfig{
		Peer: c.lifecycle, Protocol: int(wire.ProtocolVersion), LockPath: paths.StartLockPath(),
		Ensure: spawn.EnsureRunning, Timeout: timeout,
	}, c.lifecycleBuild)
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
		Dial: wire.UnixDialer(c.socket), Build: c.businessBuild,
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
