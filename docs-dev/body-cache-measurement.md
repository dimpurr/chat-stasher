# Body cache: measured evidence for release criteria ① and ⑥

This note records two facts the body cache (ADR-034, implemented in
`crates/chat-stasher/src/body_cache.rs`) rests on: **what the cache key actually is**
(criterion ⑥) and **how reading a session fares wall-clock, cold vs warm, on a local
archive** (criterion ①). Both were measured; neither is assumed.

## Criterion ⑥ — the cache key is the ciphertext's identity, not the data blob id

ADR-034 predicted a body cache keyed by the **data blob id**, and its premise
experiment asked whether those ids are the same across keys. The premise test
(`crates/chat-stasher/tests/w120_premise_test.rs`) measures the answer with two
local repositories, same content, two keys: **a data blob id is the plaintext
content hash of the chunk it names — free of the key; pack ids are hashes of the
*encrypted* pack — disjoint under different keys.** What is *not* identical across
keys is the **set** of ids: the Rabin polynomial that picks the chunk boundaries is
drawn at random by `init` and stored in that repository's own config, so two
separately-initialised repositories cut the same content in different places as soon
as a file passes the chunker's minimum size. One failing run reported, in one
repository only, the two SHA-256s of a single shard body's halves.

That makes ADR-034's preferred, cross-destination-sharing branch *true at the blob
level* only where two destinations cut the content identically — which two
independently-initialised destinations generally do not — and the cache cannot key
by it anyway. The only seam this crate can install below
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

## Criterion ① against a remote-like destination

The local measurement above answers a narrower question than the criterion asks.
ADR-034's ① is "the machine with a quota reads the same large session noticeably
faster the second time (107 MB: 23 s → < 2 s)", and the 23 s in it is a
**download** figure — W117 measured a real remote at ~4.7 MB/s. Locally there is
nothing to download, which is exactly why the win there is 4%. This section is the
same read against a destination that behaves like a remote, measured 2026-09-29 on
one machine, release build.

### Method

- A synthetic archive of 102,400,000 B (102.4 MB) of plaintext in 50 sealed shards,
  pushed through the project's own sealed-shard + `push` path — the same fixture
  shape and size as the local section above.
- The destination is `opendal:sftp`, the string production ships, with the options a
  real one takes (`endpoint`, `user`, `key`, `known_hosts_strategy`) plus `root`. No
  real host, credential or conversation is involved.
- The remote is a local OpenSSH `sshd` on a loopback port, with a throwaway host key
  and client key generated into a temp dir for the run.
- The link is a TCP proxy in the test: the ssh client dials the proxy, the proxy
  forwards to `sshd`, and every **download** byte is charged to a leaky bucket shared
  by all connections at **4.73 MB/s** — `107e6 / 22.6`, W117's own pair of numbers.
  The upload direction is unmetered, because the quantity being modelled is a
  download rate.
- The read therefore travels `chat-stasher → opendal:sftp → ssh → TCP proxy → sshd`.
  No layer of the real path is skipped, and the thing throttled is the thing the
  cache is supposed to save: bytes on a socket.
- Cache: a `[cache]` section with quota `2 GiB` (10% = 215 MiB > the session, so it
  may be stored). Cold = the cache cleared first; warm = served from a filled cache.
  Five runs of each; the middle elapsed value is the median.

### Result

| kind | per-run elapsed (ms) | median (ms) | link bytes (median) | ssh connections | body hits | body misses |
|---|---|---|---|---|---|---|
| cold | 23489, 22649, 22649, 22566, 22543 | **22649** | 76.89 MB | 4 | 0/96 | 96/96 |
| warm | 2939, 2862, 2849, 3253, 3077 | **2939** | 0.32 MB | 2 | 96/96 | 0/96 |

**Warm is 13.0% of cold** — the same session, read twice, 22.6 s → 2.9 s.

The link counters are what make the table readable, and they are the part that would
catch a harness that had quietly stopped throttling:

- A cold read moved 76.89 MB across the metered link. At 4.73 MB/s that is **16.3 s
  of the 22.6 s** it took.
- A warm read moved 0.32 MB — **0.07 s of the 2.9 s**.

So the cache removed essentially all of the transfer — 76.89 MB becomes 0.32 MB. The
cold figure should not be read as "the link" on its own, though: 16.3 s of its 22.6 s
is metered transfer, and the remaining 6.4 s is local work — ssh session setup,
decryption, and writing 76.89 MB into the cache — of which the warm read still pays
2.9 s.

### What this says about criterion ①

The criterion's direction holds, and strongly: **the second read of an unchanged
session is 7.7× faster, because it downloads nothing.**

Its specific figure does not hold here. "23 s → < 2 s" is not reached: the warm read
is 2.9 s, and 2.87 s of that is *not* the link. That residual is the `opendal:sftp`
path's own per-read cost — every SFTP session starts its own ssh `ControlMaster`
(`src/reap.rs:3`), and the proxy carried 4 connections for a cold read and 2 for a
warm one, against 0 for a local archive. Cross-check: the local measurement above
put a warm read of the same size at 763 ms, so roughly 2 s of the warm read here is
sftp-path overhead that no hit rate can remove.

That is a floor rather than a regression — the cache is doing its job (zero body
misses, 0.32 MB on the wire), and the remaining time is paid before the first body is
requested.

### The fixture is not incompressible

The local section above describes the fixture as 2,048,000 "incompressible" bytes per
shard, "so ciphertext ≈ plaintext". Measured, that overstates it: the body generator
draws uniformly from a 62-character alphabet, so a byte carries 5.954 bits and cannot
be represented in fewer than 0.744 of its length. zstd reaches 0.746, and the cold
read above pulled 76.89 MB for 102.4 MB of plaintext — 0.751 including pack and index
overhead. The entropy floor is why that ratio is stable rather than noise.

Nothing in either section's conclusion turns on this. The local cold/warm medians and
the remote link-byte counters were both taken with this same fixture, and both say
what they say. It changes only how a link-byte figure should be read: as ciphertext,
not as "the session's size".

### Reproducing it

`#[ignore]`d like its local counterpart, and for a stronger reason: besides the
~102 MB fixture it starts an ssh server and a throttling proxy, so it needs OpenSSH's
`sshd` and keeps a listening socket for the length of the run.

```sh
cargo test --release --test w246_remote_cache_test -- --ignored --nocapture
```

It prints the per-run table, both medians, and the per-run link bytes and ssh
connection counts.