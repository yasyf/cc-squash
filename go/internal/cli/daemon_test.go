package cli

import (
	"log/slog"
	"testing"
)

func TestDaemonLogLevel(t *testing.T) {
	tests := []struct {
		name    string
		raw     string
		want    slog.Level
		wantErr bool
	}{
		{"unset", "", slog.LevelInfo, false},
		{"debug", "debug", slog.LevelDebug, false},
		{"upper", "WARN", slog.LevelWarn, false},
		{"offset", "error+2", slog.LevelError + 2, false},
		{"garbage", "loud", 0, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := daemonLogLevel(tt.raw)
			if (err != nil) != tt.wantErr {
				t.Fatalf("daemonLogLevel(%q) error = %v, wantErr %v", tt.raw, err, tt.wantErr)
			}
			if !tt.wantErr && got != tt.want {
				t.Errorf("daemonLogLevel(%q) = %v, want %v", tt.raw, got, tt.want)
			}
		})
	}
}
