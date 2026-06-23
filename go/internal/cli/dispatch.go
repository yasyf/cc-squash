package cli

// helpVersionFlags are the root flags cobra owns: help and version must resolve
// to ccs's own output, never forward through to claude.
var helpVersionFlags = map[string]bool{
	"-h": true, "--help": true,
	"-v": true, "--version": true,
}

// InjectRun rewrites a bare ccs invocation that leads with a claude flag into an
// explicit `ccs run`, so `ccs --resume` behaves like `ccs run --resume`. It fires
// only when args[0] is a flag other than ccs's own help/version flags; bare
// `ccs`, subcommands, positional prompts/typos (preserving cobra's suggestions),
// and `ccs --help`/`--version` all pass through unchanged.
func InjectRun(args []string) []string {
	if len(args) == 0 {
		return args
	}
	first := args[0]
	if first == "" || first[0] != '-' || helpVersionFlags[first] {
		return args
	}
	return append([]string{"run"}, args...)
}
