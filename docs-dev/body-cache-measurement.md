# Body cache: measured evidence for release criteria ① and ⑥

This note records two facts the body cache (ADR-034, implemented in
`crates/chat-stasher/src/body_cache.rs`) rests on: **what the cache key actually is**
(criterion ⑥) and **how reading a session fares wall-clock, cold vs warm, on a local
archive** (criterion ①). Both were measured; neither is assumed.

## Criterion ⑥ — the cache key is the ciphertext's identity, not the data blob id

ADR-034 predicted a body cache keyed by the **data blob id**, and its premise
experiment asked whether those ids are the same across keys. The premise test
(`crates/chat-stasher/tests/w120_premise_test.rs`) measures the answer with two
local repositories, same content, two keys: **data blob ids are plaintext content
hashes — identical under different keys; pack ids are hashes of the *encrypted*
pack — disjoint under different keys.**

That makes ADR-034's preferred, cross-destination-sharing branch *true at the blob
level* — but the cache cannot key by it. The only seam this crate can install below
`rustic_core`'s decryption layer is `ReadBackend`, where a body read arrives as
`read_partial(Pack, pack_id, offset, length)` — ciphertext coordinates, with no
plaintext id in sight. So the implemented key is `CacheKey { pack, offset, length }`
(one byte range of one encrypted pack, `body_cache.rs:398-425`), and because a pack
id is a hash of the encrypted pack it differs between destinations that do not share
a key. The cache therefore lands on ADR-034's **own documented fallback branch**, in
its stricter half: one global quota, and entries separated per destination —
implicitly, through the pack id carried in the key.

Measured consequence, matching the ADR's predicted trade-off: **two destinations with
different keys share no body-cache entries even when the content is identical.** No new
keys and no new formats are introduced.

## Criterion ① — wall-clock on a local archive, cold vs warm

Method:

- A synthetic local archive: one ~98 MiB session, written as 50 sealed shards of
  `2,048,000` incompressible bytes each (so ciphertext ≈ plaintext), through the
  project's own sealing + `push` path. No real conversation, key or destination is
  touched.
- A `[cache]` section with quota `2 GiB` (10% = 215 MiB > the ~98 MiB session, so the
  session may be stored). The cache is **not** enabled by default; this is an explicit
  opt-in section.
- A release build, driving the single-session `read` path of the real binary.
  Cold = cache cleared before the read; warm = served entirely from a filled cache.
  Five runs of each; the middle elapsed wall-clock value is the median.

Result (release build, local SSD, incompressible ~98 MiB session):

| kind  | per-run elapsed (ms)     | median (ms) | body hits | body misses |
|-------|--------------------------|-------------|-----------|-------------|
| cold  | 1374\*, 792, 783, 794, 808 | 794         | 0/95      | 95/95       |
| warm  | 757, 763, 761, 769, 763   | 763         | 95/95     | 0/95        |

\* the first read also builds the fresh repository; the median absorbs it. Cold and
warm reads return byte-identical sessions (the `concat sha256` does not change).

Interpretation, stated plainly: on a **local** archive the wall-clock win is small
(warm ≈ 96% of cold). Decryption, the body digest, and the fixed per-run repository-open
cost are paid on both paths, and locally there is no download to save. ADR-034's
"107 MB: 23 s → < 2 s" target was measured against a remote destination at ~4.7 MB/s of
download (W117) — a remote-download-dominated property. The cache's real saving there is
the removed download, and the structural fact this measurement establishes is the one
criterion ① rests on: **a warm read of an unchanged session fetches zero body bytes**
(every body lookup hits, none are missed).

## Reproducing it

The measurement above is a `#[ignore]`'d integration test, kept out of the default
`cargo test` run because the ~98 MiB fixture is too large for CI:

```sh
cargo test --release --test w243_wallclock_test -- --ignored --nocapture
```

It builds the synthetic archive, runs the cold/warm reads, and prints the per-run table
and both medians annotated as above.