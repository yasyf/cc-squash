// Package proxyseam also carries the Go server end of the proxy-v1.sock seam: the
// control plane binds proxy-v1.sock and accepts the single connection the Rust
// proxy child makes after it spawns. The child sends register once; thereafter
// the control plane writes mint/evict/shadow/kill/shutdown control frames. The
// seam is fail-open on both ends — a child that has not connected yet, a dropped
// connection leaves the daemon up while the proxy reconnects. A non-v1 frame
// closes that child session before it can mutate control state.
package proxyseam

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"log"
	"net"
	"os"
	"sync"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// ErrProxyNotConnected is returned by the Server's send methods when no proxy
// child has connected yet. It is a fail-open signal — the caller logs and
// continues rather than treating a missing data plane as fatal.
var ErrProxyNotConnected = errors.New("proxyseam: no proxy child connected")

// Server is the Go end of the proxy-v1.sock seam. It accepts one
// proxy child connection at a time, and writes control frames to whichever
// child is currently connected. A dropped child leaves the listener up so a
// respawned child can reconnect.
type Server struct {
	log *log.Logger

	ln net.Listener

	mu   sync.Mutex
	conn net.Conn
}

// NewServer binds proxy-v1.sock, removing any stale socket file first, and returns
// a Server ready to accept the proxy child. Diagnostics go to logger.
func NewServer(logger *log.Logger) (*Server, error) {
	socket := paths.ProxySocketPath()
	if err := os.MkdirAll(paths.StateDir(), 0o700); err != nil {
		return nil, err
	}
	_ = os.Remove(socket) // clear a stale socket from a prior daemon before binding
	ln, err := net.Listen("unix", socket)
	if err != nil {
		return nil, err
	}
	if err := os.Chmod(socket, 0o600); err != nil {
		_ = ln.Close()
		return nil, err
	}
	return &Server{log: logger, ln: ln}, nil
}

// Start accepts proxy child connections until ctx is cancelled or the listener
// closes. For each connection it reads the register frame, calls onRegister,
// then drains any further Rust->Go frames (Layer 1 expects none — they are
// logged and ignored). A dropped connection is logged; the loop accepts the
// next child. Run it in its own goroutine.
func (s *Server) Start(ctx context.Context, onRegister func(Register)) {
	go func() {
		<-ctx.Done()
		_ = s.ln.Close()
	}()
	for {
		conn, err := s.ln.Accept()
		if err != nil {
			if ctx.Err() != nil || errors.Is(err, net.ErrClosed) {
				return
			}
			s.log.Printf("proxyseam: accept: %v", err)
			continue
		}
		s.serveConn(ctx, conn, onRegister)
	}
}

// Close closes the listener and any live child connection.
func (s *Server) Close() error {
	s.clearConn()
	err := s.ln.Close()
	if errors.Is(err, net.ErrClosed) {
		return nil
	}
	return err
}

// Connected reports whether a proxy child connection is currently live — the
// liveness the supervisor's Policy reads to tell a registered, serving proxy
// from one that has dropped its seam.
func (s *Server) Connected() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.conn != nil
}

// serveConn admits a child only after its exact epoch-1 register frame. Rust
// sends no later frames, so any subsequent input closes the session.
func (s *Server) serveConn(ctx context.Context, conn net.Conn, onRegister func(Register)) {
	done := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			_ = conn.Close()
		case <-done:
		}
	}()
	defer close(done)
	defer conn.Close()
	scanner := bufio.NewScanner(conn)
	if !scanner.Scan() {
		if err := scanner.Err(); err != nil && !errors.Is(err, io.EOF) {
			s.log.Printf("proxyseam: register read failed: %v", err)
		}
		return
	}
	message, err := Decode(scanner.Bytes())
	if err != nil {
		s.log.Printf("proxyseam: reject register: %v", err)
		return
	}
	register, ok := message.(Register)
	if !ok {
		s.log.Printf("proxyseam: first frame is %T, want Register", message)
		return
	}
	s.setConn(conn)
	defer s.clearConn()
	s.log.Printf(
		"proxyseam: proxy registered (protocol=%d port=%d mcp_port=%d version=%s pid=%d)",
		register.Protocol, register.Port, register.MCPPort, register.Version, register.PID,
	)
	onRegister(register)
	if scanner.Scan() {
		s.log.Printf("proxyseam: reject unexpected post-register frame")
		return
	}
	if err := scanner.Err(); err != nil && !errors.Is(err, io.EOF) {
		s.log.Printf("proxyseam: proxy connection dropped: %v", err)
		return
	}
	s.log.Printf("proxyseam: proxy disconnected")
}

// SendMint hands the proxy a session token and its per-session relay config.
func (s *Server) SendMint(token string, config json.RawMessage) error {
	if len(config) == 0 {
		config = json.RawMessage("{}")
	}
	return s.send(Mint{Type: MsgMint, Protocol: ProtocolVersion, Token: token, Config: config})
}

// SendEvict tells the proxy to drop the session bound to token.
func (s *Server) SendEvict(token string) error {
	return s.send(Evict{Type: MsgEvict, Protocol: ProtocolVersion, Token: token})
}

// SendShadow toggles the proxy's shadow mode.
func (s *Server) SendShadow(on bool) error {
	return s.send(Shadow{Type: MsgShadow, Protocol: ProtocolVersion, On: on})
}

// SendKill toggles the proxy's kill switch.
func (s *Server) SendKill(on bool) error {
	return s.send(Kill{Type: MsgKill, Protocol: ProtocolVersion, On: on})
}

// SendGc tells the proxy to sweep its ref store down to the reachable set.
func (s *Server) SendGc() error {
	return s.send(Gc{Type: MsgGc, Protocol: ProtocolVersion})
}

// SendShutdown tells the proxy to step down.
func (s *Server) SendShutdown() error {
	return s.send(Shutdown{Type: MsgShutdown, Protocol: ProtocolVersion})
}

// send marshals a frame and writes it to the connected child under the write
// lock. With no child connected it returns ErrProxyNotConnected — the fail-open
// signal the caller logs and continues past.
func (s *Server) send(msg any) error {
	frame, err := Encode(msg)
	if err != nil {
		return err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.conn == nil {
		return ErrProxyNotConnected
	}
	_, err = s.conn.Write(frame)
	return err
}

func (s *Server) setConn(conn net.Conn) {
	s.mu.Lock()
	s.conn = conn
	s.mu.Unlock()
}

// clearConn drops the connection from the write side and closes it. Safe to
// call twice (serveConn's defer and Close): the second call sees a nil conn.
func (s *Server) clearConn() {
	s.mu.Lock()
	conn := s.conn
	s.conn = nil
	s.mu.Unlock()
	if conn != nil {
		_ = conn.Close()
	}
}
