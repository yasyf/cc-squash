package supervisor

import (
	"testing"
)

func TestNormalizeProxyVersion(t *testing.T) {
	cases := []struct {
		id, display, want string
	}{
		{"dev identity is unchanged", "dev", "dev"},
		{"release v-prefix is stripped", "v0.1.0", "0.1.0"},
		{"bare semver is unchanged", "0.1.0", "0.1.0"},
		{"commit suffix is dropped", "v0.4.2 (abc1234)", "0.4.2"},
		{"prerelease round-trips", "v0.2.0-rc.1", "0.2.0-rc.1"},
	}
	for _, c := range cases {
		t.Run(c.id, func(t *testing.T) {
			if got := normalizeProxyVersion(c.display); got != c.want {
				t.Fatalf("normalizeProxyVersion(%q) = %q, want %q", c.display, got, c.want)
			}
		})
	}
}
