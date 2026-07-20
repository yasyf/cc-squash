// Package config is the sole parser of the user's cc-squash config.toml. It
// reads ~/.cc-squash/config.toml, mirrors the seam contract the Rust proxy
// deserializes, and emits JSON carrying ONLY the keys the user actually set, so
// an absent file or an unset key leaves the engine's compiled-in defaults
// untouched. An absent config is engine defaults, not an error.
package config

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"

	toml "github.com/pelletier/go-toml/v2"

	"github.com/yasyf/cc-squash/go/internal/paths"
)

// Economics mirrors the [economics] table of the seam contract. Every field is a
// pointer so an unset key is omitted from the marshalled JSON and the engine
// keeps its default.
type Economics struct {
	NPVFloor   *float64 `toml:"npv_floor" json:"npv_floor,omitempty"`
	TTLAutoS   *float64 `toml:"ttl_auto_s" json:"ttl_auto_s,omitempty"`
	TTLForcedS *float64 `toml:"ttl_forced_s" json:"ttl_forced_s,omitempty"`
}

// Policy mirrors the [policy] table of the seam contract. Every field is a
// pointer so an unset key is omitted from the marshalled JSON and the engine
// keeps its default.
type Policy struct {
	RecencyWindowN    *int `toml:"recency_window_n" json:"recency_window_n,omitempty"`
	HumanVerbatimMax  *int `toml:"human_verbatim_max" json:"human_verbatim_max,omitempty"`
	PreGateMinChars   *int `toml:"pre_gate_min_chars" json:"pre_gate_min_chars,omitempty"`
	CacheHintCap      *int `toml:"cache_hint_cap" json:"cache_hint_cap,omitempty"`
	LookbackPositions *int `toml:"lookback_positions" json:"lookback_positions,omitempty"`
}

// Config is the seam contract: the [economics] and [policy] tables the proxy
// reads. Both sections are pointers so an entirely-unset section is omitted from
// the JSON the seam carries.
type Config struct {
	SchemaVersion int        `toml:"schema_version" json:"-"`
	Economics     *Economics `toml:"economics" json:"economics,omitempty"`
	Policy        *Policy    `toml:"policy" json:"policy,omitempty"`
}

// Load reads ~/.cc-squash/config.toml and returns the seam JSON the daemon
// pushes to the proxy on every mint. The retained source-of-truth configuration
// must explicitly declare schema version 1; there is no legacy reader or
// automatic migration.
func Load() (json.RawMessage, error) {
	data, err := os.ReadFile(paths.ConfigPath())
	if errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("%s is required with schema_version = 1", paths.ConfigPath())
	}
	if err != nil {
		return nil, err
	}
	var cfg Config
	decoder := toml.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&cfg); err != nil {
		return nil, err
	}
	if cfg.SchemaVersion != 1 {
		return nil, fmt.Errorf("%s has schema_version = %d, want 1; transfer the retained settings manually", paths.ConfigPath(), cfg.SchemaVersion)
	}
	return json.Marshal(cfg)
}
