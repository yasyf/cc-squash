package cli

import (
	"slices"
	"testing"
)

func TestRootCmdRegistersSubcommands(t *testing.T) {
	root := NewRootCmd()
	var got []string
	for _, c := range root.Commands() {
		got = append(got, c.Name())
	}
	slices.Sort(got)
	want := []string{stopRuntimeCommand, "daemon", "doctor", "env", "gc", "kill", "logs", "run", "service", "shadow", "status", "stop", "url"}
	if !slices.Equal(got, want) {
		t.Fatalf("subcommands = %v, want %v", got, want)
	}
	stopRuntime, _, err := root.Find([]string{stopRuntimeCommand})
	if err != nil || stopRuntime == root || !stopRuntime.Hidden {
		t.Fatalf("hidden stop runtime command = %+v, err = %v", stopRuntime, err)
	}
}

func TestRootCmdUseAndVersion(t *testing.T) {
	root := NewRootCmd()
	if root.Use != "ccs" {
		t.Errorf("root.Use = %q, want ccs", root.Use)
	}
	if root.Version == "" {
		t.Error("root.Version is empty; version wiring is missing")
	}
	if !root.SilenceUsage || !root.SilenceErrors {
		t.Errorf("SilenceUsage=%v SilenceErrors=%v, want both true", root.SilenceUsage, root.SilenceErrors)
	}
}
