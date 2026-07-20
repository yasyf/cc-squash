// Package proxyseam defines the wire envelopes for the proxy-v1.sock seam — the
// single persistent unix socket the Go control plane binds and the Rust proxy
// connects to. Transport is line-delimited JSON, one object per line; every
// message is an envelope with a "type" discriminator. The Go side encodes
// control messages and decodes the proxy's register frame. A frame outside the
// exact epoch-1 contract is rejected.
package proxyseam

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
)

// ProtocolVersion is the exact Go/Rust proxy seam epoch.
const ProtocolVersion = 1

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
	MsgGc       MsgType = "gc"
	MsgShutdown MsgType = "shutdown"
)

// Register is the proxy's announcement, sent once right after it binds its TCP
// ports: the bound 127.0.0.1 relay port, the SECOND listener's MCP port (the
// rmcp cc_squash_retrieve server), its semver, and its OS pid.
type Register struct {
	Type     MsgType `json:"type"`
	Protocol int     `json:"protocol"`
	Port     int     `json:"port"`
	MCPPort  int     `json:"mcp_port"`
	Version  string  `json:"version"`
	PID      int     `json:"pid"`
}

// Mint hands the proxy a session token and its per-session relay config.
type Mint struct {
	Type     MsgType         `json:"type"`
	Protocol int             `json:"protocol"`
	Token    string          `json:"token"`
	Config   json.RawMessage `json:"config"`
}

// Evict tells the proxy to drop a session by token.
type Evict struct {
	Type     MsgType `json:"type"`
	Protocol int     `json:"protocol"`
	Token    string  `json:"token"`
}

// Shadow toggles the proxy's shadow mode.
type Shadow struct {
	Type     MsgType `json:"type"`
	Protocol int     `json:"protocol"`
	On       bool    `json:"on"`
}

// Kill toggles the proxy's kill switch.
type Kill struct {
	Type     MsgType `json:"type"`
	Protocol int     `json:"protocol"`
	On       bool    `json:"on"`
}

// Gc tells the proxy to sweep its ref store: it computes the reachable set from
// every session's staged refs and evicts the rest under the grace/byte budget.
type Gc struct {
	Type     MsgType `json:"type"`
	Protocol int     `json:"protocol"`
}

// Shutdown tells the proxy to step down.
type Shutdown struct {
	Type     MsgType `json:"type"`
	Protocol int     `json:"protocol"`
}

// Encode marshals a seam message and appends the framing newline.
func Encode(msg any) ([]byte, error) {
	if err := validateMessage(msg); err != nil {
		return nil, err
	}
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
	case MsgGc:
		return unmarshal[Gc](line)
	case MsgShutdown:
		return unmarshal[Shutdown](line)
	default:
		return nil, fmt.Errorf("proxyseam: unknown message type %q", head.Type)
	}
}

func unmarshal[T any](line []byte) (any, error) {
	var msg T
	decoder := json.NewDecoder(bytes.NewReader(line))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&msg); err != nil {
		return nil, err
	}
	if err := decoder.Decode(&struct{}{}); err != io.EOF {
		return nil, errors.New("proxyseam: trailing JSON")
	}
	if err := validateMessage(msg); err != nil {
		return nil, err
	}
	return msg, nil
}

func validateMessage(msg any) error {
	var protocol int
	switch value := msg.(type) {
	case Register:
		protocol = value.Protocol
		if value.Type != MsgRegister || value.Port < 1 || value.MCPPort < 1 ||
			value.Version == "" || value.PID < 1 {
			return errors.New("proxyseam: invalid register")
		}
	case Mint:
		protocol = value.Protocol
		var config map[string]json.RawMessage
		if value.Type != MsgMint || value.Token == "" ||
			json.Unmarshal(value.Config, &config) != nil || config == nil {
			return errors.New("proxyseam: invalid mint")
		}
	case Evict:
		protocol = value.Protocol
		if value.Type != MsgEvict || value.Token == "" {
			return errors.New("proxyseam: invalid evict")
		}
	case Shadow:
		protocol = value.Protocol
		if value.Type != MsgShadow {
			return errors.New("proxyseam: invalid shadow")
		}
	case Kill:
		protocol = value.Protocol
		if value.Type != MsgKill {
			return errors.New("proxyseam: invalid kill")
		}
	case Gc:
		protocol = value.Protocol
		if value.Type != MsgGc {
			return errors.New("proxyseam: invalid gc")
		}
	case Shutdown:
		protocol = value.Protocol
		if value.Type != MsgShutdown {
			return errors.New("proxyseam: invalid shutdown")
		}
	default:
		return errors.New("proxyseam: unsupported message")
	}
	if protocol != ProtocolVersion {
		return fmt.Errorf("proxyseam: protocol must be %d", ProtocolVersion)
	}
	return nil
}
