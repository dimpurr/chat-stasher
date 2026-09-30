# Recover a lost machine's conversations

<!-- RELEASE GATE: this page describes cumulative `search` and `read`, the archive-only activity-index rebuild, the per-format full-text index, and the three-state `index` reporting, all merged after 0.5.0-rc.2. Ship it with the release that carries them. Step 6 is written as blocked on purpose: keep it written as blocked until `verify --level l3` can run without the archiving machine. -->

A machine that archived to your destination is not in front of you any more. It was sold, wiped, reinstalled, or it stopped booting. Its conversations are still in your destination, encrypted with the key you kept, and this page is the path from "I no longer have that machine" to "I have the conversations I wanted, and a checksum that says they came back intact".

Nothing below runs on the lost machine and nothing below needs it. Every command reads your destination and writes nothing to it, so you can run all of them before you decide what to take out.

> [!NOTE]
> Every transcript on this page comes from a synthetic archive built by `scripts/use-case-fixture.sh` in the repository: the machines, session ids, dates and conversation lines are invented, and the destination is an ordinary folder at `/tmp/uc/offsite`. Read `offsite` as your destination's name and `/tmp/uc/...` as that throwaway folder, and the commands are the ones you would run.
>
> The session ids are derived from the invented names, so they come out the same on every build. The snapshot ids do not: they depend on when the archive was made, so a snapshot you build yourself will have a different one from the one quoted here.

## What this needs

- **The destination**, declared in your config the way the lost machine declared it, and **its key file**. A destination is a full copy of an archive, and the key file is the only thing that opens it. There is no key recovery: if that file is gone, nothing on this page can help ([troubleshooting.md](../troubleshooting.md#i-lost-the-master-key)).
- **The right key file — this is the step people get wrong.** Each archive copy has its own key. The destination `offsite` is opened by `masterkey-offsite.json`, not by the lost machine's `masterkey.json`, which opens that machine's *local* archive and is not used here at all. Restore the destination's own key at the same path it had on the lost machine — `~/.local/share/chat-stasher/masterkey-<destination>.json`, unless that machine's config set `key_file` to somewhere else ([config.md → `[destinations.<name>]`](../config.md#destinationsname) documents `key_file`) — and put it back at exactly that path. Getting this wrong is not silent: the tool exits `3` and says `cannot read masterkey file … (lost key?)`, which means nothing was read, not that the archive is empty. [troubleshooting.md](../troubleshooting.md#i-lost-the-master-key) is the short version.
- **`chat-stasher`, on any computer.** Reading an archive needs the tool, not the machine that wrote it. If you are on a replacement machine, [troubleshooting.md](../troubleshooting.md#i-want-to-read-my-archive-on-another-machine) is the two-minute version of getting a destination declared here.
- **Patience for one step.** One command below reads every byte of the destination rather than its metadata, and says so where it appears.

## Before you start: can this archive be read at all?

Three commands, all read-only, and all of them are already true or already false before you touch anything else.

```sh
chat-stasher overview --destination offsite
```

`overview` prints one block per machine that has ever pushed, and the totals behind them. On this page's example archive it prints:

```
machines 2 · harnesses 1 · conversations 5 (of 5 observations) · total lines 6

machine × harness matrix (per cell: sessions · lines · [span])
                        claude-code                                  unknown
attic-laptop (01234567) 4 sessions·5 lines·[2026-03-02~2026-03-09]   1
desk-mini (unnamed)     1 sessions·1 lines·[2026-03-09~2026-03-09]   0
```

Two machines, one of them a laptop you no longer have. The same command prints a machine that has a snapshot but **no** readable activity index as `index missing` rather than leaving it out, because a machine this run could not read the index for is not a machine that archived nothing ([how-it-works.md](../how-it-works.md#two-tiers-metadata-and-content) explains what the index is).

If `overview` exits `3`, the destination could not be read in full and nothing below will be any better. Fix that first: [troubleshooting.md](../troubleshooting.md#a-command-exits-3).

Then the two checks that read the archive itself rather than its index:

```sh
chat-stasher verify --destination offsite --level l1
chat-stasher verify --destination offsite --level l2
```

```
[verify] L1 structure     ok=true  findings=0   errors=0   warns=0   (read_data=false) took 61.2495ms
[verify] L2 content       ok=true  findings=0   errors=0   warns=0   (read_data=true) took 120.758333ms
```

`l1` checks the structure without downloading anything. `l2` downloads and re-hashes every pack, which is the strongest statement you can make about a destination that no longer has its machine. Both work here, and both are cheap on a small archive and slow on a large one.

## The machine is called something you have to look up

The name you read on this page is not always the name the tool uses. A machine's **partition** is what every command takes as `--machine`, and it is either a hostname it declared or a random 128-bit id generated on its first run. `overview --json --summary` gives you both, per machine:

```sh
chat-stasher overview --destination offsite --json --summary
```

```json
  "machines": [
    {
      "display": "attic-laptop (01234567)",
      "health": "healthy",
      "machine": "0123456789abcdef0123456789abcdef",
      "newest_snapshot_unix": 1790738957,
      "silence_after_days": 6
    },
    {
      "display": "desk-mini (unnamed)",
      "health": "healthy",
      "machine": "desk-mini",
      "newest_snapshot_unix": 1790738957,
      "silence_after_days": 7
    }
  ],
```

`machine` is what you pass to `--machine`. `display` is what a human reads, and `attic-laptop (01234567)` means the partition is a generated id and somebody wrote a name for it. Where `unnamed` appears, nobody has, and the id is all there is.

A name is written down by a machine that is still here, because a name reaches the destination the same way everything else does: through a stage and a `push`. On the surviving machine:

```sh
chat-stasher machine-label --stage ~/stash/chat-stasher/stage \
  --target 0123456789abcdef0123456789abcdef --label "attic-laptop"
```

```
[machine-label] target  : 0123456789abcdef0123456789abcdef
[machine-label] label   : attic-laptop
[machine-label] writer  : desk-mini
[machine-label] written : 1790738869
[machine-label] file    : …/stage/meta/0123456789abcdef0123456789abcdef/label-by-desk-mini.json
[machine-label] next    : the next `push` archives it with the stage
```

The last line is the part that matters: **the label is written into a stage and reaches the destination on a later `push`.** Nothing has changed until that push happens. This is the command for a machine that can no longer declare anything for itself, which is exactly the machine this page is about.

## 1. List what that machine archived

```sh
chat-stasher search --destination offsite --machine 0123456789abcdef0123456789abcdef
```

```
[search] destination  : /tmp/uc/offsite
[search] snapshots scanned: 3 of 3 in repo
[search] sessions seen: 5
[search] data blobs read: 0
[search] index files read: 2
[search] machine      : 0123456789abcdef0123456789abcdef located=3 time-unknown=1 index-trusted=true
[search] machine      : desk-mini located=0 time-unknown=0 index-trusted=true
[search] time window  : none — every session matches, whatever its time
[search] matched      : 4
  claude-c~0bac48  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=516  snapshot=5ea17418  active=1772438400..1772443800
  claude-c~19478f  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=242  snapshot=5ea17418  active=1772720400..1772720400
  claude-c~1d788b  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=194  snapshot=5ea17418  active=unknown (cannot determine conversation time: no timestamp field found within the line)
  claude-c~e459cd  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=242  snapshot=a28e04fc  active=1773054300..1773054300
[search] not matched  : 1
```

Three things in that output are the whole reason this step works on a machine that no longer exists.

- **`data blobs read: 0`.** Searching by machine, tool, id or date never downloads a conversation. It reads the snapshot trees and the small per-machine index beside them. The cost is that tree walk, so it grows with how many snapshots and sessions the archive holds, and it is nothing like the cost of reading the conversations themselves.
- **`snapshots scanned: 3 of 3 in repo`.** Every snapshot was walked, not only the newest one, and each session is reported against the snapshot that actually holds its bytes. That matters because the staged copy of a session is deleted once every destination proves it holds those bytes; those sessions then live in an **older** snapshot only. Here the first three sessions are reported against `snapshot=5ea17418`, which is the earlier of the machine's two pushes, while the fourth is against `a28e04fc`, the newer one.
- **`not matched: 1`.** The fifth session belongs to `desk-mini`, which is not the machine that was asked about.

The last column is the honest part: `active=unknown` with a reason. One session's time could not be established, and it is listed rather than dropped ([how-it-works.md](../how-it-works.md#why-unknown-is-never-0)).

## 2. Narrow by date

Dates are the conversation's own dates, never the date of the backup run.

```sh
chat-stasher search --destination offsite --machine 0123456789abcdef0123456789abcdef \
  --since 2026-03-02 --until 2026-03-04
```

```
[search] time window  : local day(s) 2026-03-02 .. 2026-03-04 inclusive (each day = 00:00:00–23:59:59 local)
[search] time unknown : 1
[search] matched      : 1
  claude-c~0bac48  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=516  snapshot=5ea17418  active=1772438400..1772443800
[search] could not be placed: 1 (NOT 'not matched' — an active filter had no answer for these)
  claude-c~1d788b  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=194  why: cannot determine conversation time: no timestamp field found within the line
[search] not matched  : 3
search: PARTIAL — the matches above are real, but `/tmp/uc/offsite` could not be read in full (0 unreadable) and 1 session(s) could not be placed in time, so there may be more
EXIT=3
```

**Read the exit code before the list.** `0` means the answer is complete, `1` means every session was read and none matched, and **`3` means the question was not fully answered**. Here one session has no derivable time, so it might or might not belong in that window; while such a session remains, this search can never return `1`. A match list plus exit `3` is not a contradiction. It is "these are real, and there may be more".

The same shape decides the negative. Asking for a day nothing was said on:

```sh
chat-stasher search --destination offsite --machine 0123456789abcdef0123456789abcdef --day 2026-03-20
```

```
search: UNKNOWN — 0 of 5 sessions matched in `/tmp/uc/offsite`, but 1 session(s) could not be placed (see the list below), so this is NOT "not there".
```

Compare that with a session that is genuinely not in this destination:

```sh
chat-stasher search --destination offsite --session zzzz9999
```

```
search: not in this destination — 0 of 5 sessions matched in `/tmp/uc/offsite` (all 3 of 3 snapshots scanned, all readable)
```

Only the second sentence is a negative, and it says why it is allowed to be one: every snapshot was walked and every one was readable. The first exits `3`; the second exits `1`. That difference is the point of the whole product, and it is the one to check when a search comes back empty.

## 3. If a date is unknown

A session's time comes from the archive's per-machine **activity index**, built from the sessions themselves at the time the machine archived them. When that index does not carry the time, nothing can place the session in a window. The repair is to rebuild the index from the archive, which needs no stage, no source machine, and writes nothing to the destination:

```sh
chat-stasher activity-index --rebuild --destination offsite \
  --machine 0123456789abcdef0123456789abcdef
```

```
[activity-index] machine   : 0123456789abcdef0123456789abcdef
[activity-index] sessions  : 4
[activity-index] snapshots : 2 of 3 scanned
[activity-index] derived   : …/Library/Caches/chat-stasher/activity/destination/offsite/0123456789abcdef0123456789abcdef/activity-v1.jsonl
[activity-index] elapsed   : 22ms
[activity-index] read-only : the destination was not modified — a partition's index is written only by the machine that owns it
```

It walks every snapshot of that machine, reads the conversations out of the archive, and writes a **derived** index under this machine's cache folder. `snapshots: 2 of 3 scanned` counts that machine's snapshots against every snapshot in the destination, which is how you can tell it looked at the whole of that machine's history and not only its newest push. Deleting the derived file costs nothing but the next rebuild.

> [!IMPORTANT]
> **What this step does not do yet.** The commands on this page read a machine's index **out of the destination**, and this rebuild deliberately does not write there: a partition's index is written only by the machine that owns it. So the recovered times are in the derived file and in the rebuild's own report, and they are not yet what `overview` or a date search answers with. The rebuild says as much in its last line, and it leaves the archive byte-identical, which is the property you want from the one command on this page that could have damaged it.

## 4. Narrow by what was said

Searching the text needs a local index, built on this machine from the archive. It is plaintext, it lives in this machine's cache folder, and it is never uploaded ([how-it-works.md](../how-it-works.md#local-helpers-caches-and-the-index)).

```sh
chat-stasher index build --destination offsite
chat-stasher index check --destination offsite
```

```
[index] documents=5 read=5 indexed=5 not_indexable=0 empty_body=0 unchanged=0 removed=0 bytes_read=1438
[index] state=valid documents=5 last_build_indexed=5 last_build_empty_body=0 last_build_bytes_read=1438 last_build_unread_lines=0
[index] not_indexable=0 (none)
```

Then a query:

```sh
chat-stasher search --destination offsite --machine 0123456789abcdef0123456789abcdef --text kettleloop
```

```
search: mode=fts
[search] mode=fts
[search] selected=4
[search] matched=1
[search] index_covered=4
[search] index_missing=0
[search] index_not_indexable=0 (none)
[search] index_truncated=false
[search] metadata_unreadable_parts=0
[search] unplaceable_sessions=0
[search] metadata_answer_complete=true
```

`matched=1` is the answer, and `index_covered=4` is the statement about how much of the question was answered: all four of that machine's sessions have text the index can vouch for. `index_missing` and `index_not_indexable` are the two ways that can fail, and they are kept apart on purpose.

- **`index_missing`** counts sessions the index holds nothing for, which is what an index built before the machine's later sessions looks like. Run `index build` again.
- **`index_not_indexable`** counts sessions whose archived format this build cannot read at all, named by format. Those sessions were not searched, so a `0` says nothing about them, and a view holding one exits `3`.
- A session whose format *was* read and holds no conversation is an **empty body**, which is a measurement and not a failure.

None of this is needed to find a session you can already name: `search --destination <name> --session <prefix>` matches on metadata only, with no index build at all, which is the fallback when the text search is short of coverage.

**The CLI tells you how many matched, not which.** The dashboard's search page lists them with the matching line and a link into the reader:

```sh
chat-stasher ui --destination offsite
```

## 5. Read one back

```sh
chat-stasher read --destination offsite --machine 0123456789abcdef0123456789abcdef \
  --session claude-code.0123456789abcdef0123456789abcdef.aaaa0001-1111-4111-8111-111111111111
```

```
[read] body cache     : on (quota=2147483648 B)
[read] machine        : 0123456789abcdef0123456789abcdef
[read] repo           : /tmp/uc/offsite
[read] ssh reap       : ON
[read] body cache     : hits=0 misses=1 corrupt=0 stored=1 skipped_too_large=0 skipped_session=0 errors=0 usage=310 B in 1 entries
[read] shards (seq order):
  000001.jsonl  sha256=5dad533ebab6e1903567933dbc84cf7f1c755f0e3a7b384e9bfb774a7eb49e1a
[read] concat len      : 516  sha256=5dad533ebab6e1903567933dbc84cf7f1c755f0e3a7b384e9bfb774a7eb49e1a
[read] expected src    : not compared (no --stage given; the archive's own sha256 above is the answer)
EXIT=0
```

That digest is the checksum of the conversation as the archive holds it, shard by shard and then concatenated. Two things it is not:

- **It is not a proof that the archive is intact.** It proves what came back is what the archive holds. Whether the archive still holds what was pushed is a different question, answered in the next step.
- **It does not need the archiving machine's stage.** `--stage` is optional and only compares the shards on *your* disk against the archive's digest, which is useful when you still have the machine that wrote them and useless when you do not.

**The id has to be the archive's own one, and it starts with the tool.** A session is stored under `<tool>.<partition>.<native id>`, and that whole string is its name. This is worth knowing before you query for one, because the two obvious mistakes both produce a confident-looking answer rather than an error.

```sh
chat-stasher search --destination offsite --session aaaa0002
```

```
search: not in this destination — 0 of 5 sessions matched in `/tmp/uc/offsite` (all 3 of 3 snapshots scanned, all readable)
```

Nothing is wrong with the archive: `--session` matches from the start of the stored name, and the stored name does not start with `aaaa0002`. Prefix the tool and the same query finds it:

```sh
chat-stasher search --destination offsite --session claude-code.0123456789abcdef0123456789abcdef.aaaa0002
```

```
[search] matched      : 1
  claude-c~19478f  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=242  snapshot=5ea17418  active=1772720400..1772720400
[search] not matched  : 4
```

`read --session` is stricter still: it takes the whole stored name and nothing less. A native id on its own, or a prefix of the stored name, exits `3` with "holds no shards in any of the snapshots of machine …", which means the tool could not resolve what you gave it, not that the session is absent.

The one command that prints every whole name is this one:

```sh
chat-stasher read --all-machines --destination offsite --full-ids
```

```
[read] mode           : all-machines (every snapshot, cumulatively)
[read] snapshots read : 3 (get_all_snapshots lists every snapshot file)
[read] machines       : 2
  machine 0123456789abcdef0123456789abcdef snapshot=a28e04fc23b2… sessions=4
    session claude-code.0123456789abcdef0123456789abcdef.aaaa0001-1111-4111-8111-111111111111 shards=1   bytes=516        sha256=5dad533ebab6e1903567933dbc84cf7f1c755f0e3a7b384e9bfb774a7eb49e1a
    session claude-code.0123456789abcdef0123456789abcdef.aaaa0002-2222-4222-8222-222222222222 shards=1   bytes=242        sha256=94947f508317bbf6eefd341001acafacff6d1cb148ed2280c406e577ac3559a7
    session claude-code.0123456789abcdef0123456789abcdef.aaaa0003-3333-4333-8333-333333333333 shards=1   bytes=194        sha256=bc84e761d77e2dc60418e9da71cbffe63e5fac2d060b5486343cb0d091717d75
    session claude-code.0123456789abcdef0123456789abcdef.aaaa0004-4444-4444-8444-444444444444 shards=1   bytes=242        sha256=d9fa20cdbe00bf41b262a798e709396f1cd8ac7442c2e7fe3c3ffcee5509fd45
  machine desk-mini        snapshot=1ceede3aa50f… sessions=1
    session claude-code.desk-mini.bbbb0001-1111-4111-8111-111111111111 shards=1   bytes=244        sha256=e81be9dba647506bf9be632afa6f2fd21e53afe128a15adcf06ade1f3f729d38
```

(That run's header and wall-clock lines are left out, and its two 64-character snapshot ids are shortened to fit the page; every machine and session row is verbatim.)

This is the complete enumeration, and it is also the expensive one: unlike `search`, it fetches and hashes **every shard it lists**, so on a real archive it is a full download, not a listing. The cheap way to get the same names is to take the sessions out first and read the manifest, which is the last step below.

## 6. Check the archive itself

Reading one session back proves that session. Checking the destination proves the container:

| Level | What it does | Needs the lost machine? |
|---|---|---|
| `verify --level l1` | Checks the repository's structure. Cheap, reaches the network, downloads no payload. | No |
| `verify --level l2` | Downloads and re-hashes every pack. | No |
| `verify --level l3` | Reconciles every session against the manifest the archiving machine derived from its own stage. | **Yes. This step is blocked, and this is why.** |

`l1` and `l2` are the two runs at the top of this page, and both of them still apply here. `l3` is the one that would tie the archive to the sessions the machine actually had, and it cannot run for a machine you no longer have. It derives what it expects from a **stage**, and a machine you have sold or wiped has no stage left here. Run it with a stage that is not the archiving machine's and it says so rather than pretending:

```sh
chat-stasher verify --destination offsite --level l3
chat-stasher verify --destination offsite --level l3 --stage <any local folder>
```

```
verify: `--stage` is required for level l3 / all
EXIT=2

verify: L3 reconcile: L3 baseline: stage body is gone and no stored summary exists under <any local folder>/meta; there is nothing to reconcile against
[verify] RESULT         : INCOMPLETE (1 level(s) unreadable)
EXIT=3
```

`3` is the right answer here and not a failure of the archive: the run could not finish reading, so nothing is proven either way. What you have instead is `l2` for the container and a per-session digest for each conversation you take out, which is the next step.

## 7. Take them out

```sh
chat-stasher export --destination offsite --machine 0123456789abcdef0123456789abcdef \
  --harness claude-code --since 2026-03-01 --until 2026-03-10 --turns user --out /tmp/uc/recovered
```

```
[export] destination  : /tmp/uc/offsite
[export] out          : /tmp/uc/recovered
[export] time window  : local day(s) 2026-03-01 .. 2026-03-10 inclusive (each day = 00:00:00–23:59:59 local)
[export] flags        : turns=user trim_to_window=false
[export] cost         : sessions=3  shards=3  data_blobs=3  plaintext_bytes=1000
[export] written      : 3 of 3 selected session(s), 1000 bytes
[export] machine      : 0123456789abcdef0123456789abcdef located=3 time-unknown=1 index-trusted=true
[export] machine      : desk-mini located=0 time-unknown=0 index-trusted=true
  claude-c~0bac48  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  lines=2/2  untimed=0  bytes=516  sha256=5dad533ebab6  turns=applied  trim=off
  claude-c~19478f  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  lines=1/1  untimed=0  bytes=242  sha256=94947f508317  turns=applied  trim=off
  claude-c~e459cd  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  lines=1/1  untimed=0  bytes=242  sha256=d9fa20cdbe00  turns=applied  trim=off
[export] time unknown : 1
[export] not placed   : 1 (NOT 'not matched' — an active filter had no answer for these, so they are absent from the export)
  claude-c~1d788b  machine=0123456789abcdef0123456789abcdef  harness=claude-code  why: cannot determine conversation time: no timestamp field found within the line
[export] manifest     : /tmp/uc/recovered/manifest.json
[export] exit status  : 3
EXIT=3
```

`export` writes exactly the sessions `search` selects for the same flags, into `<out>/<machine>/<tool>/<session-id>.jsonl`, in the tool's own format, plus a checksummed `manifest.json`. `--dry-run` prints the same plan and cost and writes nothing; run it first when the archive is large. `--turns user` keeps only your own messages, where the tool's format makes that certain, and every other file is written whole with the manifest saying so, so nothing is ever silently dropped.

**Exit `3` here is not a failure of the export.** Three sessions were written and their checksums are in the manifest; a fourth could not be placed in time, so it is absent, and the run says which filter had no answer for it. `0` means it wrote something and answered for everything, `1` means it read everything and selected nothing.

Read the manifest before you trust the directory:

```sh
python3 -m json.tool /tmp/uc/recovered/manifest.json
```

It carries, per session, the machine, the tool, the full session id, the first and last message time, the shard count, the bytes written, the SHA-256 of the written file and the filters that were applied; and at the top level what no filter could place, which machines' indexes could not be read, and what could not be written. The `sha256` there is the same digest `read` printed, so one session can be checked twice by two different commands.

## When the conversation is not there

Six different answers, and they call for six different things. The second column is what tells them apart, and only one of the six is a statement that the conversation is not there.

| What you see | What it means | What to do |
|---|---|---|
| `search` exits `1` and says *not in this destination*, with `N of N snapshots scanned, all readable` | Every snapshot was walked and this destination does not hold it. A proven absence. | That copy of the machine's history does not include it. Try another destination, if you have one. |
| `search` exits `3`, whatever it printed | The question was not fully answered: a session could not be placed in time, or a snapshot could not be walked. | Read the list under `could not be placed`. A `0` here means nothing. |
| `overview` lists the machine as `index missing` | The machine pushed a snapshot but no activity index could be read from it. Its sessions are not in *that* listing, which is not the same as its having none. | `activity-index --rebuild` (step 3) derives one from the archive. |
| `search --text` reports `index_missing` above `0` | The local full-text index predates those sessions, so their text was never searched. | `index build --destination <name>` again; it re-reads only what changed. |
| `search --session <native id>` exits `1` and says *not in this destination*, but `search --machine` lists the session | The id you quoted is not how the session is named: the stored name starts with the tool. | Put the tool in front of it. Step 5 has the two forms side by side. |
| The destination cannot be reached | Nothing was read, so nothing is known. | Fix the destination first ([troubleshooting.md](../troubleshooting.md#destinations)). An unreachable destination is never an empty one. |

If the conversation was never archived in the first place, nothing here recovers it. The archive holds what the machine pushed before it went; a session created after the last push is not in it.

## What is not possible yet

- **`verify --level l3` for a lost machine.** It reconciles against the archiving machine's stage, and that stage is gone. Step 6 above is written as blocked rather than left out, and a page that dropped it would be promising a proof the tool cannot make.
- **The recovered activity index is not read by `overview` or a date search yet.** The rebuild derives it correctly and writes it under this machine's cache; those commands still read the archive's copy. Step 3 has the whole of it.
- **There is no key recovery.** If the destination's key file is gone, its archive is gone. Keep more than one copy of the key, somewhere other than the disk it protects. And keep a copy of **each** key — the local archive's and every destination's are different files ([setup.md](../setup.md#3-the-master-key)), and a backup that has only one of them recovers only the copy that one opens.
- **Nothing writes sessions back into a tool's own folder.** `export` writes files in the tool's format, and what you do with them is your call.
- **There is no redaction.** Anything you export carries whatever the conversation carried.

## Next

- **Turn what you recovered into something reusable.** [Turn old conversations into a skill](skill-from-history.md) starts where this page's export ends.
- **Why the archive behaves this way.** [how-it-works.md](../how-it-works.md) covers the two reading tiers, the partition rule, and why an unknown is never a zero.
- **How to re-run this page's examples.** The archive quoted above is synthetic, and `scripts/use-case-fixture.sh` in the repository builds it, prints its path, and can be deleted when you are done.
