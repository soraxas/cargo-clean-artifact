# Dependency considerations

- 2026-10-01: Added a direct `serde_json` dependency for reading Cargo fingerprint
  files. It was already present transitively through `cargo_metadata`. Fingerprints
  are Cargo implementation details, so detection skips unrecognized/malformed
  records and reports inferred commands, not an exact historical invocation.
  Detection uses `cargo metadata --no-deps --offline` to identify workspace targets
  and the configured target directory without resolving dependencies or building.
