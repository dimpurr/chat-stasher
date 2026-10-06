# Inbox contract fixtures

All data is synthetic. `manifest.json` pins fixture bytes by SHA-256, including
the unchanged legacy @1/@2 examples. TypeScript and Rust consume this same corpus.

`valid` is the runtime acceptance result. Where `schemaValid` differs, standard
JSON Schema accepts the structure but the runtime validator rejects a byte-level
invariant: canonical encoding, ordered bounds, decoded length or slice SHA-256.
Fidelity is captured metadata; registry declarations are not an input here.

Harness-file validation is available independently of archive wiring. Sealing
that variant currently fails explicitly before touching stage; the extension's
existing producer continues to emit @2.
