package emulator

import "time"

// oracleClock is the fixed wall time of the oracle run; the Rust test uses the same instant.
var oracleClock = time.Unix(1795200000, 123456789).UTC()

func oracleNow() time.Time { return oracleClock }

var oracleCounter byte

// oracleRandRead fills b with a counter that wraps at 256, shared by every caller in order.
func oracleRandRead(b []byte) (int, error) {
	for i := range b {
		b[i] = oracleCounter
		oracleCounter++
	}

	return len(b), nil
}

// OracleTrace lists every imported symbol and selector call stub the guest reached, in order.
var OracleTrace []string

// OracleCursor is the guest heap high-water mark.
func OracleCursor(r *Runtime) uint64 { return r.cursor }
