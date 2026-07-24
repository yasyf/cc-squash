package control

import (
	"os"
	"os/exec"
	"path/filepath"
)

// proxyBinaryName is the Rust data-plane executable the daemon spawns.
const proxyBinaryName = "ccs-proxy"

// ProxyBinaryPath resolves the ccs-proxy data-plane binary the daemon spawns:
// a sibling of the running executable first (the co-released layout, where ccs
// and ccs-proxy ship side by side), falling back to the PATH.
func ProxyBinaryPath() (string, error) {
	if exe, err := os.Executable(); err == nil {
		if sibling := filepath.Join(filepath.Dir(exe), proxyBinaryName); isExecutableFile(sibling) {
			return canonicalExecutable(sibling)
		}
	}
	path, err := exec.LookPath(proxyBinaryName)
	if err != nil {
		return "", err
	}
	return canonicalExecutable(path)
}

func canonicalExecutable(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

func isExecutableFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && !info.IsDir() && info.Mode()&0o111 != 0
}
