# Per-message audit sidecar

Raw sealed bodies remain authoritative. The CLI can derive a versioned,
append-only `meta/<machine>/message-audit-v1.jsonl` sidecar from Claude Code,
Codex and OpenCode bodies. Other formats have an explicit unsupported outcome;
unparseable records have failed or partial outcomes. These outcomes never claim
zero usage. The sidecar carries keyed event joins and cwd hashes, numeric/null
usage fields, event time classifications and capture metadata. It contains no
message text, titles, raw event identifiers or raw cwd.

Audit generation requires a retained, random 32-byte join secret. Set
`CHAT_STASHER_AUDIT_KEY_FILE` to its file, **outside the stage**, on each machine
writing the archive. Use the same secret across all partitions and destination
copies of that logical archive; use a separate secret for an unrelated archive.
The CLI reads exactly 32 bytes and never generates a replacement, prints the
secret or stages the key file. Keep the file private and retain a secure backup
for rebuilds. A missing setting means the sidecar is **not built**, not empty.
A configured but unreadable or incorrectly sized key is an error. Rotation
requires a separate sidecar lineage: mixing scopes in an existing file refuses.

With that setting, collect and the shared inbox sealing sink derive audit rows
only after the body is durably sealed. Local inbox ingestion, native delivery
and future transports that call that sink share the same behavior. No remote
transport is implemented here. A failed sidecar write leaves an inbox input
unretired; retrying fills missing rows from the already sealed body before
returning a duplicate acknowledgement. Legacy incoming bundles retain their
existing sealed bytes. New `inbox@3` records preserve their tagged capture kind,
fidelity, producer metadata and encoded harness-file bytes. The sealed-record
marker is independent of the incoming bundle version.

Run the one historical backfill for each destination and the owning partition:

```sh
chat-stasher audit-backfill --destination backup --stage ./stage \
  --join-key-file ./private-audit-key
```

The migration reads the destination's cumulative shard history, including bodies
reclaimed from the stage, then scans staged bodies. It writes only derived
metadata to the stage. Publish it with the ordinary `push` command. It never
rewrites, reorders or reseals old bodies, or fills absent legacy capture facts
from registry declarations. `--machine` selects the existing partition;
`--repo` and `--key-file` can select a repository directly. The destination's
masterkey must already exist. For an explicitly stage-only scan, use
`--stage-only` instead of destination options; this cannot establish historical
archive coverage.

Progress is the sidecar's committed body-digest/position entries. Each body
commits independently under the stage lock by a synchronized temporary file and
atomic rename; the existing prefix stays verbatim. A retry rescans and adds only
missing entries. A missing/unreadable source, malformed sidecar, scope mismatch
or conflicting retry leaves coverage unknown and returns exit 3 from backfill.
A completed scan reports counts only, including incomplete extraction outcomes;
completion of a scan does not imply every format was recognized. Identical body
and event coordinates with conflicting capture metadata are refused rather than
overwriting prior facts. A sidecar can be rebuilt from the bodies with the
retained secret. On Unix directory fsync proves rename durability; Windows uses
the same directory-durability convention as the inbox shard writer.
