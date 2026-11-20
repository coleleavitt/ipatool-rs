# Code style

- Follow `AGENTS.md` code rules. Match Go names in snake_case so a reader can find the source (`lookupEntity` → `lookup_entity`).
- Prefer newtypes for validated values (`CountryCode`), enums for closed sets (`Platform`).
- No placeholder crates, no compatibility shims, no speculative options.
