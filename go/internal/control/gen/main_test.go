package main

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestGeneratedOutputCurrent(t *testing.T) {
	path := filepath.Join(moduleRoot(), "internal", "control", "protocol_gen.go")
	if err := run(true, wireSpec, path); err != nil {
		t.Fatal(err)
	}
}

func TestSchemaChangesAlterFingerprint(t *testing.T) {
	_, baseline, err := generate(wireSpec)
	if err != nil {
		t.Fatal(err)
	}
	changes := map[string]string{
		"operation": strings.Replace(wireSpec, `OpMint Op = "mint"`, `OpMint Op = "mint.changed"`, 1),
		"field":     strings.Replace(wireSpec, `Sessions int`, `Sessions int64`, 1),
		"json name": strings.Replace(wireSpec, `json:"proxy_port"`, `json:"proxy_port_changed"`, 1),
		"enum value": strings.Replace(
			wireSpec,
			`RuntimeStateHealthy RuntimeState = "healthy"`,
			`RuntimeStateHealthy RuntimeState = "ready"`,
			1,
		),
	}
	for name, definition := range changes {
		t.Run(name, func(t *testing.T) {
			_, fingerprint, err := generate(definition)
			if err != nil {
				t.Fatal(err)
			}
			if fingerprint == baseline {
				t.Fatal("wire schema change preserved fingerprint")
			}
		})
	}
}

func TestCheckRejectsStaleGeneratedOutput(t *testing.T) {
	path := filepath.Join(t.TempDir(), "protocol_gen.go")
	if err := os.WriteFile(path, []byte("stale"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := run(true, wireSpec, path); !errors.Is(err, errGeneratedOutputStale) {
		t.Fatalf("run check error = %v", err)
	}
}
