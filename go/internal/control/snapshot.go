package control

import (
	"encoding/json"
	"os"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// WriteStatus mirrors the daemon's status snapshot to disk atomically (0600),
// so `ccs status` reads it even while the control socket is mid-restart.
func WriteStatus(snap StatusSnapshot) error {
	data, err := json.Marshal(snap)
	if err != nil {
		return err
	}
	return paths.AtomicWrite(paths.StatusPath(), data, 0o600)
}

// ReadStatus reads the on-disk status snapshot the daemon last published.
func ReadStatus() (StatusSnapshot, error) {
	data, err := os.ReadFile(paths.StatusPath())
	if err != nil {
		return StatusSnapshot{}, err
	}
	var snap StatusSnapshot
	if err := json.Unmarshal(data, &snap); err != nil {
		return StatusSnapshot{}, err
	}
	return snap, nil
}
