package profilefixture

import "testing"

func TestIncrement(t *testing.T) {
	if got := Increment(1); got != 2 {
		t.Fatalf("Increment(1) = %d, want 2", got)
	}
}

