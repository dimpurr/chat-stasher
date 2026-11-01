# The end-to-end verify harness: what its counts compare

The per-harness end-to-end verification of this tool is driven by a script
(`e2e-verify.zsh`) that is not part of this repository: it drives the real
binary against real installations on the verifier's machine, with every
write isolated. What the repository keeps is the record of **what its
gates compare** — the pairing below — so a future reader of the
`found_staged` key (added by GC-3, 2026-10-13) can see the gate that key
exists to serve, rebuild the comparisons from the product's own JSON, and
know which red means what. The script reads these pairings from
`status --json` and expects nothing else from the product.

## The unit split and each gate's pairing

Since W982 one session can arrive as several records under one id — a grok
`session_docs` row and its native `chat_history.jsonl` share one id so one
conversation stays one conversation — so the verify harness compares
record-to-record and session-to-session, never across the two units:

- **`found` = records.** `status --json`'s `scanner.total_sessions`: the
  `SessionRecord`s the scan emitted. It compares only against the rerun's
  per-record `unchanged=N`; collect's changed/unchanged/reset are per record
  too, so that pairing is the one place records may be compared.
- **`collected` = staged session ids.** The session dirs the stage gained
  under the harness's isolation. It compares against the target probe's
  `found_staged` in `status --json` — the distinct staged-session count
  the product itself states, added exactly so this gate could read the
  product's own statement in place of parsing collect's human stdout for
  session tags. An unknown or not-applicable `found_staged` fails closed
  (`found-staged-unknown`, since a harness whose scan could not state its
  staged count cannot prove the staged set complete); a mismatch is
  `collected-vs-scanned`.
- **`found_store` = store rows.** The target probe's `session_count`, kept
  alongside; doctor's footprint cross-check compares store to store, never
  records.

The grok installation that motivated the `found_staged` key measured
547 records over 364 staged ids over 183 store rows on one machine — the
three units legitimately disagree, which is exactly what a gate that picked
the wrong unit would silently miss.

## The scope pairing, and its one caveat

`collected` counts **every** session dir the stage gained — every harness,
not just the one under test — while `found_staged` counts one harness's
ids. The two pair only because the harness holds its scope upstream: its
config declares every *other* registry harness's root as a nonexistent
path (a declared root that does not exist is never scanned), the harness
id list comes from the binary itself, and the analysis fails closed with
`other-harness-installed` if any other harness reports installed in
`status --json`. A future edit that lets a second harness contribute staged
dirs without reading as installed would make `collected != found_staged` a
scope difference reported as the `collected-vs-scanned` shortfall — the red
would be real, the label wrong. Under the harness's synthetic home no
second root can exist, which is why the `installed` assert is the guard the
pairing rests on.
