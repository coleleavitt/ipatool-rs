package machine

import "time"

// Deterministic randomness and time for the Rust port's oracle; must match
// `OracleHost` in crates/ipatool-sap/src/machine/oracle_tests.rs.
var oracleCounter uint64

func oracleRandRead(data []byte) (int, error) {
	for index := range data {
		data[index] = byte((oracleCounter * 2654435761) >> 13)
		oracleCounter++
	}

	return len(data), nil
}

func oracleNow() time.Time {
	return time.Unix(1800000000, 123456789)
}
