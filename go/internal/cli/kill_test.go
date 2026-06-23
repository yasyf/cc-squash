package cli

import "testing"

func TestParseToggle(t *testing.T) {
	cases := map[string]struct {
		arg     string
		want    bool
		wantErr bool
	}{
		"on":      {"on", true, false},
		"off":     {"off", false, false},
		"garbage": {"maybe", false, true},
		"empty":   {"", false, true},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			got, err := parseToggle(tc.arg)
			if (err != nil) != tc.wantErr {
				t.Fatalf("parseToggle(%q) err = %v, wantErr %v", tc.arg, err, tc.wantErr)
			}
			if got != tc.want {
				t.Fatalf("parseToggle(%q) = %v, want %v", tc.arg, got, tc.want)
			}
		})
	}
}

func TestOnOff(t *testing.T) {
	if onOff(true) != "on" {
		t.Errorf("onOff(true) = %q, want on", onOff(true))
	}
	if onOff(false) != "off" {
		t.Errorf("onOff(false) = %q, want off", onOff(false))
	}
}
