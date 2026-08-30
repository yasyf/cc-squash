package control

import (
	"encoding/hex"
	"encoding/json"
	"strings"
	"testing"
	"time"
)

func TestExactRequestShapes(t *testing.T) {
	empty, err := json.Marshal(EmptyRequest{})
	if err != nil || string(empty) != `{}` {
		t.Fatalf("empty request = %s, err = %v", empty, err)
	}
	for _, on := range []bool{false, true} {
		data, err := json.Marshal(ToggleRequest{On: on})
		if err != nil {
			t.Fatalf("marshal toggle: %v", err)
		}
		var got ToggleRequest
		if err := decodeStrict(data, &got); err != nil {
			t.Fatalf("decode toggle: %v", err)
		}
		if got.On != on {
			t.Fatalf("toggle = %t, want %t", got.On, on)
		}
	}
}

func TestResponseRoundTrip(t *testing.T) {
	want := Response{
		OK: true, Port: 50515, Token: "tok-abc",
		Status: &StatusSnapshot{
			SchemaVersion: StatusSchemaVersion,
			Version:       "1.2.3", GeneratedAt: time.Unix(1_700_000_000, 0).UTC(),
			ProxyPort: 50516, ProxyPID: 4242, Sessions: 3, Shadow: true,
		},
		Kill: true, Shadow: true,
	}
	data, err := json.Marshal(want)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got Response
	if err := decodeStrict(data, &got); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if got.Status == nil || *got.Status != *want.Status {
		t.Fatalf("status mismatch: got %+v, want %+v", got.Status, want.Status)
	}
	got.Status, want.Status = nil, nil
	if got != want {
		t.Fatalf("response mismatch: got %+v, want %+v", got, want)
	}
}

func TestResponseOmitsEmptyStatus(t *testing.T) {
	data, err := json.Marshal(Response{OK: true})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if want := `{"ok":true}`; string(data) != want {
		t.Fatalf("got %s, want %s", data, want)
	}
}

func TestHealthDetailRoundTrip(t *testing.T) {
	data, err := json.Marshal(HealthDetail{RuntimeBuild: "1.2.3"})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got HealthDetail
	if err := decodeStrict(data, &got); err != nil {
		t.Fatalf("decode: %v", err)
	}
	if got.RuntimeBuild != "1.2.3" {
		t.Fatalf("health detail = %+v", got)
	}
}

func TestWireBuildIsIndependentOfRelease(t *testing.T) {
	if len(WireSchemaFingerprint) != 64 {
		t.Fatalf("wire schema fingerprint length = %d", len(WireSchemaFingerprint))
	}
	if _, err := hex.DecodeString(WireSchemaFingerprint); err != nil {
		t.Fatalf("wire schema fingerprint = %q: %v", WireSchemaFingerprint, err)
	}
	want := "com.yasyf.cc-squash.control/" + WireSchemaFingerprint + "/v1"
	if WireBuild != want || strings.Contains(WireBuild, "1.2.3") {
		t.Fatalf("wire build = %q", WireBuild)
	}
}
