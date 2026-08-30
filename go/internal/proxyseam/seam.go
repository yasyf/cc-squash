// Package proxyseam also carries the Go server end of the proxy seam: the
// control plane spawns the Rust proxy child over a daemonkit ChannelHandoff and
// speaks this protocol on the inherited socketpair. The child sends register
// once; thereafter the control plane writes mint/evict/shadow/kill/shutdown
// control frames. The seam is fail-open on both ends — a child that has not
// registered yet, or a dropped channel, leaves the daemon up while the
// supervisor respawns. A non-v1 frame ends that child's session before it can
// mutate control state.
package proxyseam

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"log"
	"net"
	"sync"
	"time"
)

// ErrProxyNotConnected is returned by the Server's send methods when no proxy
// child channel is live. It is a fail-open signal — the caller logs and
// continues rather than treating a missing data plane as fatal.
var ErrProxyNotConnected = errors.New("proxyseam: no proxy child connected")

const writeTimeout = 2 * time.Second

type session struct {
	conn      net.Conn
	writeGate chan struct{}
}

// Server is the Go end of the proxy seam. It serves one spawned proxy child's
// handoff channel at a time and writes control frames to whichever child is
// currently registered.
type Server struct {
	log *log.Logger

	mu        sync.Mutex
	session   *session
	live      map[net.Conn]struct{}
	closed    bool
	closeOnce sync.Once
	closeErr  error
}

// NewServer returns a seam with no child attached. Diagnostics go to logger.
func NewServer(logger *log.Logger) *Server {
	return &Server{log: logger, live: map[net.Conn]struct{}{}}
}

// Serve reads conn — the parent end of one spawned proxy's handoff socketpair —
// admitting the child on its exact epoch-1 register frame, then holding the
// session until the channel ends or ctx is cancelled. Rust sends no later
// frames, so any subsequent input ends the session. Run it in its own goroutine.
func (s *Server) Serve(ctx context.Context, conn net.Conn, onRegister func(Register)) {
	if !s.track(conn) {
		_ = conn.Close()
		return
	}
	defer s.untrack(conn)
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
	if !s.setConn(conn) {
		s.log.Printf("proxyseam: reject register on a closed seam")
		return
	}
	defer s.clearConn(conn)
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
		s.log.Printf("proxyseam: proxy channel dropped: %v", err)
		return
	}
	s.log.Printf("proxyseam: proxy disconnected")
}

// Close drops every live child channel and refuses later Serve calls.
func (s *Server) Close() error {
	s.closeOnce.Do(func() {
		s.mu.Lock()
		s.closed = true
		connections := make([]net.Conn, 0, len(s.live))
		for conn := range s.live {
			connections = append(connections, conn)
		}
		s.session = nil
		s.mu.Unlock()
		var errs []error
		for _, conn := range connections {
			if err := conn.Close(); err != nil && !errors.Is(err, net.ErrClosed) {
				errs = append(errs, err)
			}
		}
		s.closeErr = errors.Join(errs...)
	})
	return s.closeErr
}

// Connected reports whether a registered proxy child's channel is still live —
// the liveness the supervisor's Policy reads to tell a registered, serving proxy
// from one that has dropped its seam.
func (s *Server) Connected() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.session != nil
}

// SendMint hands the proxy a session token and its per-session relay config.
func (s *Server) SendMint(token string, config json.RawMessage) error {
	if len(config) == 0 {
		config = json.RawMessage("{}")
	}
	return s.send(context.Background(), Mint{Type: MsgMint, Protocol: ProtocolVersion, Token: token, Config: config})
}

// SendEvict tells the proxy to drop the session bound to token.
func (s *Server) SendEvict(token string) error {
	return s.send(context.Background(), Evict{Type: MsgEvict, Protocol: ProtocolVersion, Token: token})
}

// SendShadow toggles the proxy's shadow mode.
func (s *Server) SendShadow(on bool) error {
	return s.send(context.Background(), Shadow{Type: MsgShadow, Protocol: ProtocolVersion, On: on})
}

// SendKill toggles the proxy's kill switch.
func (s *Server) SendKill(on bool) error {
	return s.send(context.Background(), Kill{Type: MsgKill, Protocol: ProtocolVersion, On: on})
}

// SendGc tells the proxy to sweep its ref store down to the reachable set.
func (s *Server) SendGc() error {
	return s.send(context.Background(), Gc{Type: MsgGc, Protocol: ProtocolVersion})
}

// SendShutdown tells the proxy to step down.
func (s *Server) SendShutdown(ctx context.Context) error {
	return s.send(ctx, Shutdown{Type: MsgShutdown, Protocol: ProtocolVersion})
}

// send marshals a frame and writes it to the registered child under the write
// gate. With no child registered it returns ErrProxyNotConnected — the fail-open
// signal the caller logs and continues past.
func (s *Server) send(ctx context.Context, msg any) error {
	frame, err := Encode(msg)
	if err != nil {
		return err
	}
	s.mu.Lock()
	current := s.session
	s.mu.Unlock()
	if current == nil {
		return ErrProxyNotConnected
	}
	writeCtx, cancel := context.WithTimeout(ctx, writeTimeout)
	defer cancel()
	select {
	case <-writeCtx.Done():
		return writeCtx.Err()
	case <-current.writeGate:
	}
	defer func() { current.writeGate <- struct{}{} }()
	s.mu.Lock()
	live := s.session == current
	s.mu.Unlock()
	if !live {
		return ErrProxyNotConnected
	}
	deadline := time.Now().Add(writeTimeout)
	if callerDeadline, ok := writeCtx.Deadline(); ok && callerDeadline.Before(deadline) {
		deadline = callerDeadline
	}
	if err := current.conn.SetWriteDeadline(deadline); err != nil {
		return err
	}
	interruptDone := make(chan struct{})
	stopInterrupt := context.AfterFunc(writeCtx, func() {
		_ = current.conn.SetWriteDeadline(time.Now())
		close(interruptDone)
	})
	_, err = current.conn.Write(frame)
	if !stopInterrupt() {
		<-interruptDone
	}
	clearErr := current.conn.SetWriteDeadline(time.Time{})
	if timeout, ok := err.(net.Error); ok && timeout.Timeout() {
		err = errors.Join(err, context.DeadlineExceeded)
	}
	return errors.Join(err, clearErr, writeCtx.Err())
}

func (s *Server) track(conn net.Conn) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return false
	}
	s.live[conn] = struct{}{}
	return true
}

func (s *Server) untrack(conn net.Conn) {
	s.mu.Lock()
	delete(s.live, conn)
	s.mu.Unlock()
}

func (s *Server) setConn(conn net.Conn) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return false
	}
	writeGate := make(chan struct{}, 1)
	writeGate <- struct{}{}
	s.session = &session{conn: conn, writeGate: writeGate}
	return true
}

// clearConn drops the connection from the write side. Safe to call twice
// (Serve's defer and Close): the second call sees another or no session.
func (s *Server) clearConn(expected net.Conn) {
	s.mu.Lock()
	if s.session != nil && s.session.conn == expected {
		s.session = nil
	}
	s.mu.Unlock()
}
