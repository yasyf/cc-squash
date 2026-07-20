package control

import (
	"errors"
	"os"
	"strconv"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// WritePort publishes the daemon's listening port to the port-file atomically
// (0600), so the CLI can reach the daemon without the socket path alone.
func WritePort(port int) error {
	if port < 1 || port > 65535 {
		return errors.New("port mirror requires a TCP port")
	}
	return paths.AtomicWrite(paths.PortFilePath(), []byte(strconv.Itoa(port)), 0o600)
}

// ReadPort reads the port the daemon published to the port-file.
func ReadPort() (int, error) {
	data, err := os.ReadFile(paths.PortFilePath())
	if err != nil {
		return 0, err
	}
	port, err := strconv.Atoi(string(data))
	if err != nil || port < 1 || port > 65535 {
		return 0, errors.New("port mirror is not the exact epoch-1 format")
	}
	return port, nil
}
