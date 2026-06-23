package proxyseam

import (
	"bytes"
	"encoding/json"
	"testing"
)

func TestEncodeAppendsNewline(t *testing.T) {
	data, err := Encode(Shutdown{Type: MsgShutdown})
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
		{"register", Register{Type: MsgRegister, Port: 50516, Version: "0.1.0", PID: 4242}},
		{"evict", Evict{Type: MsgEvict, Token: "tok-abc"}},
		{"shadow", Shadow{Type: MsgShadow, On: true}},
		{"kill", Kill{Type: MsgKill, On: true}},
		{"shutdown", Shutdown{Type: MsgShutdown}},
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
	want := Mint{Type: MsgMint, Token: "tok-abc", Config: json.RawMessage(`{"k":1}`)}
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
	if got.Type != want.Type || got.Token != want.Token || !bytes.Equal(got.Config, want.Config) {
		t.Fatalf("mint mismatch: got %+v, want %+v", got, want)
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
