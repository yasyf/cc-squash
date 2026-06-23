package control

import (
	"encoding/json"
	"testing"
	"time"
)

func TestRequestRoundTrip(t *testing.T) {
	for _, tc := range []struct {
		id  string
		req Request
	}{
		{"health", Request{Proto: ProtocolVersion, Op: OpHealth}},
		{"shadow-on", Request{Proto: ProtocolVersion, Op: OpShadow, On: true}},
		{"kill-off", Request{Proto: ProtocolVersion, Op: OpKill, On: false}},
	} {
		t.Run(tc.id, func(t *testing.T) {
			data, err := json.Marshal(tc.req)
			if err != nil {
				t.Fatalf("marshal: %v", err)
			}
			var got Request
			if err := json.Unmarshal(data, &got); err != nil {
				t.Fatalf("unmarshal: %v", err)
			}
			if got != tc.req {
				t.Fatalf("round-trip mismatch: got %+v, want %+v", got, tc.req)
			}
		})
	}
}

func TestRequestOmitsOnWhenFalse(t *testing.T) {
	data, err := json.Marshal(Request{Proto: ProtocolVersion, Op: OpKill})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if want := `{"proto":1,"op":"kill"}`; string(data) != want {
		t.Fatalf("got %s, want %s", data, want)
	}
}

func TestResponseRoundTrip(t *testing.T) {
	want := Response{
		Proto:   ProtocolVersion,
		OK:      true,
		Version: "1.2.3",
		Port:    50515,
		Token:   "tok-abc",
		Status: &StatusSnapshot{
			Proto:       ProtocolVersion,
			Version:     "1.2.3",
			GeneratedAt: time.Unix(1_700_000_000, 0).UTC(),
			ProxyPort:   50516,
			ProxyPID:    4242,
			Sessions:    3,
			Kill:        false,
			Shadow:      true,
		},
		Kill:   true,
		Shadow: true,
	}
	data, err := json.Marshal(want)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got Response
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if got.Status == nil {
		t.Fatal("status dropped in round-trip")
	}
	if *got.Status != *want.Status {
		t.Fatalf("status mismatch: got %+v, want %+v", *got.Status, *want.Status)
	}
	got.Status, want.Status = nil, nil
	if got != want {
		t.Fatalf("response mismatch: got %+v, want %+v", got, want)
	}
}

func TestResponseOmitsEmptyStatus(t *testing.T) {
	data, err := json.Marshal(Response{Proto: ProtocolVersion, OK: true})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if want := `{"proto":1,"ok":true}`; string(data) != want {
		t.Fatalf("got %s, want %s", data, want)
	}
}
