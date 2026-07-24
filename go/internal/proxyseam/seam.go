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
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"sync"
	"syscall"
	"time"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/daemonkit/drain"
	"github.com/yasyf/daemonkit/proc"
	"github.com/yasyf/daemonkit/wire"
)

// ErrProxyNotConnected is returned by the Server's send methods when no proxy
// child has connected yet. It is a fail-open signal — the caller logs and
// continues rather than treating a missing data plane as fatal.
var ErrProxyNotConnected = errors.New("proxyseam: no proxy child connected")

var errProxyAlreadyServing = errors.New("proxyseam: another proxy listener is serving")

type session struct {
	conn    net.Conn
	peer    wire.Peer
	writeMu sync.Mutex
}

// Server is the Go end of the proxy-v1.sock seam. It accepts one
// proxy child connection at a time, and writes control frames to whichever
// child is currently connected. A dropped child leaves the listener up so a
// respawned child can reconnect.
type Server struct {
	log *log.Logger

	ln     net.Listener
	lock   *os.File
	intake drain.Intake

	mu        sync.Mutex
	expected  proc.Record
	session   *session
	accepted  map[net.Conn]struct{}
	closeErr  error
	closeOnce sync.Once
}

// NewServer binds proxy-v1.sock, removing any stale socket file first, and returns
// a Server ready to accept the proxy child. Diagnostics go to logger.
func NewServer(ctx context.Context, logger *log.Logger) (*Server, error) {
	socket := paths.ProxySocketPath()
	entrant := proc.SingleEntrant{
		Socket: socket,
		Evict: func() (bool, error) {
			conn, err := net.DialTimeout("unix", socket, 100*time.Millisecond)
			if err == nil {
				_ = conn.Close()
				return false, errProxyAlreadyServing
			}
			if errors.Is(err, os.ErrNotExist) || errors.Is(err, syscall.ENOENT) || errors.Is(err, syscall.ECONNREFUSED) {
				return true, nil
			}
			return false, fmt.Errorf("proxyseam: probe listener: %w", err)
		},
	}
	ln, lock, err := entrant.Listen(ctx)
	if err != nil {
		return nil, err
	}
	return &Server{log: logger, ln: ln, lock: lock, accepted: make(map[net.Conn]struct{})}, nil
}

// ExpectProcess publishes the exact daemonkit-owned child identity allowed to
// establish the next seam session. It is called from ProcessSpec.Recorded,
// before daemonkit releases the child to exec.
func (s *Server) ExpectProcess(record proc.Record) error {
	if err := record.Validate(); err != nil {
		return fmt.Errorf("proxyseam: expected process: %w", err)
	}
	s.mu.Lock()
	s.expected = record
	current := s.session
	if current != nil && !current.peer.MatchesProcess(record) {
		s.session = nil
	}
	s.mu.Unlock()
	if current != nil && !current.peer.MatchesProcess(record) {
		_ = current.conn.Close()
	}
	return nil
}

// Start accepts proxy child connections until ctx is cancelled or the listener
// closes. For each connection it reads the register frame, calls onRegister,
// then drains any further Rust->Go frames (Layer 1 expects none — they are
// logged and ignored). A dropped connection is logged; the loop accepts the
// next child. Run it in its own goroutine.
func (s *Server) Start(ctx context.Context, onRegister func(Register)) {
	go func() {
		<-ctx.Done()
		_ = s.Close()
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
		done, err := s.intake.Admit()
		if err != nil {
			_ = conn.Close()
			return
		}
		s.trackConn(conn)
		s.serveConn(ctx, conn, onRegister)
		s.untrackConn(conn)
		done()
	}
}

// Close drains new accepts, closes the listener and authenticated session,
// settles admitted work, and releases the single-entrant listener lock.
func (s *Server) Close() error {
	s.closeOnce.Do(func() {
		s.intake.Close()
		listenerErr := s.ln.Close()
		if errors.Is(listenerErr, net.ErrClosed) {
			listenerErr = nil
		}
		s.closeConnections()
		settleErr := s.intake.Settle(context.Background())
		lockErr := s.lock.Close()
		s.closeErr = errors.Join(listenerErr, settleErr, lockErr)
	})
	return s.closeErr
}

// Connected reports whether a proxy child connection is currently live — the
// liveness the supervisor's Policy reads to tell a registered, serving proxy
// from one that has dropped its seam.
func (s *Server) Connected() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.session != nil && s.session.peer.MatchesProcess(s.expected)
}

// serveConn admits a child only after its exact epoch-1 register frame. Rust
// sends no later frames, so any subsequent input closes the session.
func (s *Server) serveConn(ctx context.Context, conn net.Conn, onRegister func(Register)) {
	unix, ok := conn.(*net.UnixConn)
	if !ok {
		s.log.Printf("proxyseam: reject non-unix peer")
		_ = conn.Close()
		return
	}
	peer, err := wire.PeerFromConn(unix)
	if err != nil {
		s.log.Printf("proxyseam: reject unidentified peer: %v", err)
		_ = conn.Close()
		return
	}
	if peer.UID != os.Geteuid() || !s.expectedMatches(peer) {
		s.log.Printf("proxyseam: reject unauthorized peer pid=%d uid=%d", peer.PID, peer.UID)
		_ = conn.Close()
		return
	}
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
	if register.PID != peer.PID {
		s.log.Printf("proxyseam: reject register pid=%d from peer pid=%d", register.PID, peer.PID)
		return
	}
	if !s.setConn(conn, peer) {
		s.log.Printf("proxyseam: reject superseded peer pid=%d", peer.PID)
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
	current := s.session
	if current == nil || !current.peer.MatchesProcess(s.expected) {
		s.mu.Unlock()
		return ErrProxyNotConnected
	}
	s.mu.Unlock()
	current.writeMu.Lock()
	defer current.writeMu.Unlock()
	s.mu.Lock()
	live := s.session == current && current.peer.MatchesProcess(s.expected)
	s.mu.Unlock()
	if !live {
		return ErrProxyNotConnected
	}
	_, err = current.conn.Write(frame)
	return err
}

func (s *Server) expectedMatches(peer wire.Peer) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return peer.MatchesProcess(s.expected)
}

func (s *Server) trackConn(conn net.Conn) {
	s.mu.Lock()
	s.accepted[conn] = struct{}{}
	s.mu.Unlock()
}

func (s *Server) untrackConn(conn net.Conn) {
	s.mu.Lock()
	delete(s.accepted, conn)
	s.mu.Unlock()
}

func (s *Server) closeConnections() {
	s.mu.Lock()
	connections := make([]net.Conn, 0, len(s.accepted))
	for conn := range s.accepted {
		connections = append(connections, conn)
	}
	s.session = nil
	s.mu.Unlock()
	for _, conn := range connections {
		_ = conn.Close()
	}
}

func (s *Server) setConn(conn net.Conn, peer wire.Peer) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !peer.MatchesProcess(s.expected) {
		return false
	}
	s.session = &session{conn: conn, peer: peer}
	return true
}

// clearConn drops the connection from the write side and closes it. Safe to
// call twice (serveConn's defer and Close): the second call sees a nil conn.
func (s *Server) clearConn(expected net.Conn) {
	s.mu.Lock()
	current := s.session
	if current != nil && (expected == nil || current.conn == expected) {
		s.session = nil
	} else {
		current = nil
	}
	s.mu.Unlock()
	if current != nil {
		_ = current.conn.Close()
	}
}
