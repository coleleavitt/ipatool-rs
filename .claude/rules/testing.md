# Testing

- Default tests are offline and deterministic. Network tests are `#[ignore]` with a reason string.
- URL builders are tested against the exact Go URL string.
- Parsers are tested with sanitized fixtures, including a negative case.
- The emulator (phase 3+) is tested against byte-exact vectors recorded from the Go implementation.
