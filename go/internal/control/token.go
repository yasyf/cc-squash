package control

import (
	"crypto/rand"
	"encoding/base64"
)

// tokenBytes is the entropy of a minted session token before base64 encoding.
const tokenBytes = 32

// Token is an opaque per-session secret the daemon mints and hands the proxy so
// it can attribute and gate a relayed session.
type Token string

// Mint generates a fresh session token from 32 bytes of crypto/rand entropy,
// base64url-encoded without padding.
func Mint() (Token, error) {
	buf := make([]byte, tokenBytes)
	if _, err := rand.Read(buf); err != nil {
		return "", err
	}
	return Token(base64.RawURLEncoding.EncodeToString(buf)), nil
}
