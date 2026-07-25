package control

import (
	"fmt"
	"os"
	"testing"
	"time"

	"github.com/yasyf/daemonkit/trust"
)

const runtimeStartupTimeout = 5 * time.Second

func TestMain(m *testing.M) {
	if handled, err := trust.RunVerifierChild(os.Args[1:], os.Stdout); handled {
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		os.Exit(0)
	}
	os.Exit(m.Run())
}
