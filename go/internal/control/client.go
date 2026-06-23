package control

import (
	"bufio"
	"encoding/json"
	"errors"
	"net"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/fusekit/proc"
)

// ErrDaemonUnavailable means the control socket could not be reached.
var ErrDaemonUnavailable = errors.New("cc-squash daemon not running")

// Client is a short-lived connection to the control-plane socket.
type Client struct {
	socket string
}

// NewClient returns a client for the default control socket path.
func NewClient() *Client { return &Client{socket: paths.SocketPath()} }

// Available reports whether the control socket accepts a connection.
func (c *Client) Available() bool {
	conn, err := net.DialTimeout("unix", c.socket, 500*time.Millisecond)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}

// do sends one request and reads one response, stamping the protocol version
// and bounding the round-trip with timeout.
func (c *Client) do(req Request, timeout time.Duration) (Response, error) {
	conn, err := net.DialTimeout("unix", c.socket, 500*time.Millisecond)
	if err != nil {
		return Response{}, ErrDaemonUnavailable
	}
	defer func() { _ = conn.Close() }()
	_ = conn.SetDeadline(time.Now().Add(timeout))

	req.Proto = ProtocolVersion
	if err := json.NewEncoder(conn).Encode(req); err != nil {
		return Response{}, err
	}
	var resp Response
	if err := json.NewDecoder(bufio.NewReader(conn)).Decode(&resp); err != nil {
		return Response{}, err
	}
	return resp, nil
}

// Health probes the daemon's liveness and version.
func (c *Client) Health() (Response, error) {
	return c.do(Request{Op: OpHealth}, 2*time.Second)
}

// Status fetches the daemon's full status snapshot.
func (c *Client) Status() (Response, error) {
	return c.do(Request{Op: OpStatus}, 5*time.Second)
}

// Mint asks the daemon to mint a fresh session token for the proxy.
func (c *Client) Mint() (Response, error) {
	return c.do(Request{Op: OpMint}, 3*time.Second)
}

// Kill toggles the proxy kill switch.
func (c *Client) Kill(on bool) (Response, error) {
	return c.do(Request{Op: OpKill, On: on}, 2*time.Second)
}

// Shadow toggles the proxy's shadow mode.
func (c *Client) Shadow(on bool) (Response, error) {
	return c.do(Request{Op: OpShadow, On: on}, 2*time.Second)
}

// Shutdown asks the daemon to step down and release the socket. An OK reply
// means it accepted; use WaitGone to confirm the socket went dead.
func (c *Client) Shutdown() (Response, error) {
	return c.do(Request{Op: OpShutdown}, 2*time.Second)
}

// WaitGone polls until the socket stops accepting connections or timeout
// elapses, reporting whether it went dead.
func (c *Client) WaitGone(timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		conn, err := net.DialTimeout("unix", c.socket, 200*time.Millisecond)
		if err != nil {
			return true
		}
		_ = conn.Close()
		time.Sleep(100 * time.Millisecond)
	}
	return false
}

// EnsureRunning returns true if the daemon is reachable, auto-spawning a
// detached `ccs daemon` and waiting up to timeout for its socket if it is not.
// A second instance is harmless: the daemon refuses to start when the socket is
// already owned. The control plane can always spawn itself (CanHost returns
// nil); the child's stdout and stderr append to the daemon log.
func (c *Client) EnsureRunning(timeout time.Duration) bool {
	return proc.Spawn{
		Socket:    c.socket,
		Args:      []string{"daemon"},
		Timeout:   timeout,
		LogPath:   paths.LogPath(),
		Available: c.Available,
		CanHost:   func() error { return nil },
	}.EnsureRunning() == nil
}
