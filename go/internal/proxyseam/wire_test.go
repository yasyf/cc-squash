package proxyseam

import (
	"bytes"
	"encoding/json"
	"testing"
)

func TestEncodeAppendsNewline(t *testing.T) {
	data, err := Encode(Shutdown{Type: MsgShutdown, Protocol: ProtocolVersion})
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	if !bytes.HasSuffix(data, []byte("\n")) {
		t.Fatalf("frame missing trailing newline: %q", data)
	}
	if bytes.Count(data, []byte("\n")) != 1 {
		t.Fatalf("frame must hold exactly one newline: %q", data)
	}
}

func TestRoundTripDispatch(t *testing.T) {
	for _, tc := range []struct {
		id  string
		msg any
	}{
		{"register", Register{Type: MsgRegister, Protocol: ProtocolVersion, Port: 50516, MCPPort: 50517, Version: "0.1.0", PID: 4242}},
		{"evict", Evict{Type: MsgEvict, Protocol: ProtocolVersion, Token: "tok-abc"}},
		{"shadow", Shadow{Type: MsgShadow, Protocol: ProtocolVersion, On: true}},
		{"kill", Kill{Type: MsgKill, Protocol: ProtocolVersion, On: true}},
		{"gc", Gc{Type: MsgGc, Protocol: ProtocolVersion}},
		{"shutdown", Shutdown{Type: MsgShutdown, Protocol: ProtocolVersion}},
	} {
		t.Run(tc.id, func(t *testing.T) {
			frame, err := Encode(tc.msg)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			got, err := Decode(bytes.TrimRight(frame, "\n"))
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			if got != tc.msg {
				t.Fatalf("dispatch mismatch: got %#v (%T), want %#v (%T)", got, got, tc.msg, tc.msg)
			}
		})
	}
}

// TestRoundTripMint covers Mint separately: its Config json.RawMessage is a
// []byte, so the struct is not comparable with == and needs field-wise checks.
func TestRoundTripMint(t *testing.T) {
	want := Mint{Type: MsgMint, Protocol: ProtocolVersion, Token: "tok-abc", Config: json.RawMessage(`{"k":1}`)}
	frame, err := Encode(want)
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	decoded, err := Decode(bytes.TrimRight(frame, "\n"))
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	got, ok := decoded.(Mint)
	if !ok {
		t.Fatalf("decoded to %T, want Mint", decoded)
	}
	if got.Type != want.Type || got.Protocol != want.Protocol || got.Token != want.Token || !bytes.Equal(got.Config, want.Config) {
		t.Fatalf("mint mismatch: got %+v, want %+v", got, want)
	}
}

// TestRegisterDecodesMCPPort pins the Rust->Go register-frame contract: the
// proxy serializes its SECOND listener's port under the JSON key "mcp_port", and
// Go must read it onto Register.MCPPort. The literal here is the shape seam.rs
// emits.
func TestRegisterDecodesMCPPort(t *testing.T) {
	decoded, err := Decode([]byte(`{"type":"register","protocol":1,"port":50516,"mcp_port":50517,"version":"0.1.0","pid":4242}`))
	if err != nil {
		t.Fatalf("decode register: %v", err)
	}
	reg, ok := decoded.(Register)
	if !ok {
		t.Fatalf("decoded %T, want Register", decoded)
	}
	if reg.Port != 50516 || reg.MCPPort != 50517 {
		t.Fatalf("register = port %d mcp_port %d, want 50516/50517", reg.Port, reg.MCPPort)
	}
}

func TestProtocolIsExact(t *testing.T) {
	for _, frame := range []string{
		`{"type":"shutdown"}`,
		`{"type":"shutdown","protocol":2}`,
		`{"type":"shutdown","protocol":1,"legacy":true}`,
		`{"type":"shutdown","protocol":1}{}`,
	} {
		if _, err := Decode([]byte(frame)); err == nil {
			t.Fatalf("Decode accepted %s", frame)
		}
	}
	if _, err := Encode(Shutdown{Type: MsgShutdown}); err == nil {
		t.Fatal("Encode accepted a frame without protocol 1")
	}
	if _, err := Encode(Shutdown{Type: MsgKill, Protocol: ProtocolVersion}); err == nil {
		t.Fatal("Encode accepted a concrete message with the wrong type")
	}
}

func TestDecodeUnknownType(t *testing.T) {
	if _, err := Decode([]byte(`{"type":"bogus"}`)); err == nil {
		t.Fatal("expected error on unknown message type")
	}
}

func TestDecodeMalformedJSON(t *testing.T) {
	if _, err := Decode([]byte(`{not json`)); err == nil {
		t.Fatal("expected error on malformed JSON")
	}
}
