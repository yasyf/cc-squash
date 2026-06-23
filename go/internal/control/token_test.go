package control

import "testing"

func TestMintNonEmpty(t *testing.T) {
	tok, err := Mint()
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	if tok == "" {
		t.Fatal("minted an empty token")
	}
}

func TestMintUnique(t *testing.T) {
	const n = 1000
	seen := make(map[Token]struct{}, n)
	for range n {
		tok, err := Mint()
		if err != nil {
			t.Fatalf("mint: %v", err)
		}
		if _, dup := seen[tok]; dup {
			t.Fatalf("duplicate token: %s", tok)
		}
		seen[tok] = struct{}{}
	}
}
