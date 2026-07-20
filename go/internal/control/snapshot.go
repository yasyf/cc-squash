package control

import (
	"encoding/json"
	"errors"
	"os"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// WriteStatus mirrors the daemon's status snapshot to disk atomically (0600),
// so `ccs status` reads it even while the control socket is mid-restart.
func WriteStatus(snap StatusSnapshot) error {
	if snap.SchemaVersion != StatusSchemaVersion {
		return errors.New("status snapshot writer requires schema version 1")
	}
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
	if err := decodeStrict(data, &snap); err != nil {
		return StatusSnapshot{}, err
	}
	if snap.SchemaVersion != StatusSchemaVersion {
		return StatusSnapshot{}, errors.New("status snapshot is not schema version 1")
	}
	return snap, nil
}
