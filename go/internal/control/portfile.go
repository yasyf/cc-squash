package control

import (
	"os"
	"strconv"
	"strings"

	"github.com/yasyf/cc-squash/go/internal/paths"
	"github.com/yasyf/fusekit/state"
)

// WritePort publishes the daemon's listening port to the port-file atomically
// (0600), so the CLI can reach the daemon without the socket path alone.
func WritePort(port int) error {
	return state.AtomicWrite(paths.PortFilePath(), []byte(strconv.Itoa(port)), 0o600)
}

// ReadPort reads the port the daemon published to the port-file.
func ReadPort() (int, error) {
	data, err := os.ReadFile(paths.PortFilePath())
	if err != nil {
		return 0, err
	}
	return strconv.Atoi(strings.TrimSpace(string(data)))
}
