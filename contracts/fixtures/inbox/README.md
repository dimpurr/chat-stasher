# Inbox contract fixtures

All data is synthetic. `manifest.json` pins fixture bytes by SHA-256, including
the unchanged legacy @1/@2 examples. TypeScript and Rust consume this same corpus.

`valid` is the runtime acceptance result. Where `schemaValid` differs, standard
JSON Schema accepts the structure but the runtime validator rejects a byte-level
invariant: canonical encoding, ordered bounds, decoded length or slice SHA-256.
Fidelity is captured metadata; registry declarations are not an input here.

The shared corpus is checked against JSON Schema, TypeScript and Rust validators.
Valid bundles also pass sealing, encrypted archive readback and audit-sidecar
rebuild; invalid bundles must fail before stage writes. Harness-file slices retain
their exact decoded bytes and capture metadata. Legacy @1/@2 sealed records keep
their existing format. The extension's existing producer continues to emit @2.
