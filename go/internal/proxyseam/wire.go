// Package proxyseam defines the wire envelopes for the proxy.sock seam — the
// single persistent unix socket the Go control plane binds and the Rust proxy
// connects to. Transport is line-delimited JSON, one object per line; every
// message is an envelope with a "type" discriminator. The Go side encodes
// control messages and decodes the proxy's register frame; both ends fail open
// when the seam drops, so a malformed line is the caller's to log and skip, not
// fatal here.
package proxyseam

import (
	"encoding/json"
	"fmt"
)

// MsgType discriminates a seam envelope.
type MsgType string

// Seam message types. Register flows Rust -> Go once after the proxy binds; the
// rest flow Go -> Rust any time after.
const (
	MsgRegister MsgType = "register"
	MsgMint     MsgType = "mint"
	MsgEvict    MsgType = "evict"
	MsgShadow   MsgType = "shadow"
	MsgKill     MsgType = "kill"
	MsgShutdown MsgType = "shutdown"
)

// Register is the proxy's announcement, sent once right after it binds its TCP
// port: the bound 127.0.0.1 port, its semver, and its OS pid.
type Register struct {
	Type    MsgType `json:"type"`
	Port    int     `json:"port"`
	Version string  `json:"version"`
	PID     int     `json:"pid"`
}

// Mint hands the proxy a session token and its per-session relay config. Config
// is passed through opaquely (RawConfig) so an unknown shape never errors here.
type Mint struct {
	Type   MsgType         `json:"type"`
	Token  string          `json:"token"`
	Config json.RawMessage `json:"config"`
}

// Evict tells the proxy to drop a session by token.
type Evict struct {
	Type  MsgType `json:"type"`
	Token string  `json:"token"`
}

// Shadow toggles the proxy's shadow mode.
type Shadow struct {
	Type MsgType `json:"type"`
	On   bool    `json:"on"`
}

// Kill toggles the proxy's kill switch.
type Kill struct {
	Type MsgType `json:"type"`
	On   bool    `json:"on"`
}

// Shutdown tells the proxy to step down.
type Shutdown struct {
	Type MsgType `json:"type"`
}

// Encode marshals a seam message and appends the framing newline.
func Encode(msg any) ([]byte, error) {
	data, err := json.Marshal(msg)
	if err != nil {
		return nil, err
	}
	return append(data, '\n'), nil
}

// Decode peeks the "type" field of one seam frame, then unmarshals it into the
// matching struct, returned as the concrete type behind any.
func Decode(line []byte) (any, error) {
	var head struct {
		Type MsgType `json:"type"`
	}
	if err := json.Unmarshal(line, &head); err != nil {
		return nil, err
	}
	switch head.Type {
	case MsgRegister:
		return unmarshal[Register](line)
	case MsgMint:
		return unmarshal[Mint](line)
	case MsgEvict:
		return unmarshal[Evict](line)
	case MsgShadow:
		return unmarshal[Shadow](line)
	case MsgKill:
		return unmarshal[Kill](line)
	case MsgShutdown:
		return unmarshal[Shutdown](line)
	default:
		return nil, fmt.Errorf("proxyseam: unknown message type %q", head.Type)
	}
}

func unmarshal[T any](line []byte) (any, error) {
	var msg T
	if err := json.Unmarshal(line, &msg); err != nil {
		return nil, err
	}
	return msg, nil
}
