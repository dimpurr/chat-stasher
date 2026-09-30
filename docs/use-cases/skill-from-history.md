# Turn old conversations into a skill

<!-- RELEASE GATE: this page describes the local full-text index, `search --text` and `search --scan`, and `export` as they stand after 0.5.0-rc.2. Ship it with the release that carries them. -->

You have an archive, and by now it has years of conversations in it. Some of that is worth more than a record: a procedure you worked out over a dozen sessions, a checklist that stopped you making the same mistake twice, a house style you explained once and kept re-explaining. You want that out, written down, in a form you can use again.

The archive will not summarise itself, and this page does not pretend otherwise. What it does is get the right sessions out of the archive, in their original text, with a checksum on each, so an agent can read them and you can check what it read.

> [!NOTE]
> Every transcript on this page comes from a synthetic archive built by `scripts/use-case-fixture.sh` in the repository: the machines, session ids, dates and conversation lines are invented, and the destination is an ordinary folder at `/tmp/uc/offsite`. Read `offsite` as your destination's name and `/tmp/uc/...` as that throwaway folder, and the commands are the ones you would run.

## What this needs

- **A readable archive, declared as a destination in your config, and its key file.** If you have not made one yet, [start.md](../start.md) is the shortest path to that.
- **The local full-text index.** It is built on this machine from the archive, it is plaintext, and it is never uploaded. The next step builds it.
- **Something to read the exported files.** An agent, a text editor, or you. That part is outside chat-stasher, and step 5 says what to hand over.

## Read this before you start

This is the page that ends outside the tool, so the privacy shape matters more here than anywhere else on this site.

- **The exported files are plaintext.** They are the conversations, decrypted, in the tool's own format. Once `export` has run, they are ordinary files on your disk, and every tool that can read a file can read them.
- **The local index is plaintext too.** It lives in this machine's cache folder, one per destination. `chat-stasher index clear --destination <name>` deletes it, and nothing else depends on it ([how-it-works.md](../how-it-works.md#local-helpers-caches-and-the-index)).
- **There is no redaction.** Nothing here removes secrets, names or anything else from a conversation before it leaves. Choose what you export, and choose where you put it.
- **The archive itself is not touched.** Everything below reads it. The archive stays append-only, and deleting the export folder costs you nothing but the work of exporting again.

## 1. Build the local index

```sh
chat-stasher index build --destination offsite
chat-stasher index check --destination offsite
```

```
[index] documents=5 read=5 indexed=5 not_indexable=0 empty_body=0 unchanged=0 removed=0 bytes_read=1438
[index] state=valid documents=5 last_build_indexed=5 last_build_empty_body=0 last_build_bytes_read=1438 last_build_unread_lines=0
[index] not_indexable=0 (none)
```

The index covers what the archive held when it was built, so run `index build` again after the machines push more. It re-reads only what changed, and the two counters that can go wrong are named rather than folded into the total:

- **`last_build_not_indexable`** counts sessions whose archived format this build cannot read. They are named by format (`not indexable: sqlite`), they are never counted as indexed, and a query over a view holding one exits `3`, because a session that was not read is not a session the query is absent from.
- **`last_build_empty_body`** counts sessions whose format *was* read and which hold no conversation. That is a measurement about the session, not a failure of the index.

`index check` reads nothing from the archive, so it is the cheap way to see how far behind you are.

## 2. Find the sessions worth keeping

Two different questions, and it is worth keeping them apart.

**Is the material in here at all?** A query over the index answers that, and states how much of the archive it looked at:

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

Read that as two separate claims. `matched=1` is the answer. `index_covered=4`, `index_missing=0` and `index_not_indexable=0` are the statement about the question: every session the filter selected had text the index could vouch for, so this `1` is the whole answer and not a floor. Change any of the three and the meaning changes:

| What you see | What it means | What to do |
|---|---|---|
| `index_missing` above `0` | The index was built before those sessions arrived, so their text was never searched. | `index build` again. |
| `index_not_indexable` above `0` | Those sessions' archived format cannot be read by this build. A `0` says nothing about them, and the command exits `3`. | Nothing yet, for those sessions. `--scan` reads the conversations themselves, and counts a format it cannot read the same way. |
| `metadata_answer_complete=false` | A session could not be placed in time, so a date filter had no answer for it. | The query still answered; the caveat is named in the output. |

A query shorter than three characters cannot be answered from the index, which matches runs of three or more. Today the CLI prints that as `matched=0` plus a suggestion line, `[search] suggestion: use a query of at least 3 characters`, and — when the index otherwise covers the selection — exits `1`, the same code as a search that read everything and matched nothing. On a short query the safe reading is the suggestion line, not the `0`: the index was never asked, so the zero is not a statement that the material is absent. `--scan` is the way around it: it reads the selected conversations themselves and matches case-insensitively, so it answers a shorter query and does not depend on the index being current. It is slower, and it downloads what it reads.

**Which sessions are they?** The CLI tells you how many matched, not which. The dashboard's search page lists them, shows the matching line, and links each one into the reader:

```sh
chat-stasher ui --destination offsite
```

Type the same query into its search box. Each result carries the machine, the tool, the session's short id and its last message time; underneath is the line that matched, and a link that opens the conversation in the reader.

**And if you already know which session it was**, you do not need any of this. A note, a commit message or an old citation usually carries the conversation's id; a prefix of the stored name finds it on metadata alone, with no index build at all:

```sh
chat-stasher search --destination offsite --session claude-code.0123456789abcdef0123456789abcdef.aaaa0002
```

```
…
[search] matched      : 1
  claude-c~19478f  machine=0123456789abcdef0123456789abcdef  harness=claude-code  shards=1  bytes=242  snapshot=5ea17418  active=1772720400..1772720400
…
```

(The run's header lines and its closing `not matched  : 4` count are left out; the matched row is verbatim.)

The id is the archive's own: it starts with the tool, so `--session aaaa0002` on its own finds nothing and says so. [Recover a lost machine's conversations](lost-machine.md#5-read-one-back) has that trap written out in full, and it applies here too.

## 3. Export just those

`export` writes exactly the sessions `search` selects for the same filters, so the filters are the selecting. Price the run before you pay for it:

```sh
chat-stasher export --destination offsite --machine 0123456789abcdef0123456789abcdef \
  --harness claude-code --since 2026-03-01 --until 2026-03-10 --turns user \
  --out /tmp/uc/skills/the-rig --dry-run
```

```
[export] destination  : /tmp/uc/offsite
[export] out          : /tmp/uc/skills/the-rig
[export] time window  : local day(s) 2026-03-01 .. 2026-03-10 inclusive (each day = 00:00:00–23:59:59 local)
[export] flags        : turns=user trim_to_window=false dry_run=true
[export] cost         : sessions=3  shards=3  data_blobs=3  plaintext_bytes=1000
[export] dry run      : nothing was written, no directory was created
[export] would select : 3 session(s), 1 not matched, 1 could not be placed
[export] exit status  : 3
EXIT=3
```

The dry run carries the same exit code the real run would, which is the point of running it.

Drop `--dry-run` to do it:

```sh
chat-stasher export --destination offsite --machine 0123456789abcdef0123456789abcdef \
  --harness claude-code --since 2026-03-01 --until 2026-03-10 --turns user --out /tmp/uc/skills/the-rig
```

```
[export] destination  : /tmp/uc/offsite
[export] out          : /tmp/uc/skills/the-rig
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
[export] manifest     : /tmp/uc/skills/the-rig/manifest.json
[export] exit status  : 3
EXIT=3
```

The flags worth knowing, because each one changes what the agent sees:

| Flag | What it does |
|---|---|
| `--turns user` | Keeps only your own messages, where the tool's format makes that certain. Elsewhere every line is written and the readout says `turns=not-requested`, so nothing is dropped silently. |
| `--trim-to-window` | With a date filter, also drops lines timestamped outside it. |
| `--since` / `--until` / `--day` | The conversation's own dates, not the dates of the backup runs. |
| `--machine` / `--harness` | Narrows by which machine and which tool. |
| `--force` | Writes into a directory that already has files. Without it, `--out` must be empty or absent. |

**Exit `3` on an export is not a failed export.** Here three sessions were written and their checksums are in the manifest; a fourth could not be placed in time, so it is absent and the run says why. That is the same three-state rule as everywhere else: `0` wrote something and answered for everything, `1` read everything and selected nothing, `3` what was written is real and incomplete.

Two limits worth stating plainly, because they decide what you can select:

- **`export` cannot select by text.** It takes the same flags `search` takes, and `--text` is not one of them. Use the text query to find out whether the material is there and how much of it, then select with machine, tool and dates.
- **A session with no derivable time is not selected by a date filter**, and the export says so rather than guessing. If the sessions you want are in that list, widen the window until they are not.

## 4. Check the manifest

Every export writes `<out>/manifest.json` beside the session files. It is the record of what happened, and it is the reason an export can be trusted without re-reading the archive.

```sh
python3 -m json.tool /tmp/uc/skills/the-rig/manifest.json
```

Per session it carries the machine, the tool, the full session id, the first and last message time, the shard count, the bytes written, the SHA-256 of the written file, and the filters that were applied. At the top level it carries the sessions no filter could place, the machines whose activity index could not be read, and the sessions that could not be written. Nothing is left out of the directory silently: one of those lists says why it is not there.

The layout is one file per session, in the tool's own format, byte-identical to what `read` returns for it:

```
/tmp/uc/skills/the-rig/
  0123456789abcdef0123456789abcdef/
    claude-code/
      claude-code.0123456789abcdef0123456789abcdef.aaaa0001-1111-4111-8111-111111111111.jsonl
      claude-code.0123456789abcdef0123456789abcdef.aaaa0002-2222-4222-8222-222222222222.jsonl
      claude-code.0123456789abcdef0123456789abcdef.aaaa0004-4444-4444-8444-444444444444.jsonl
  manifest.json
```

That same `sha256` is what `read` prints for the session, so the file on disk and the archive can be compared by two different commands without either one trusting the other.

## 5. Hand the files to your agent

This step is outside chat-stasher, and the tool has no command for it. Two things are worth doing deliberately.

**Point the agent at the directory, not at the archive.** The export is the whole input, so the agent never needs your key, your destination or the tool.

**Say what you want, and ask for the sessions back.** A prompt that names the shape of the answer gets a better one:

> Read every `.jsonl` file under `/tmp/uc/skills/the-rig/`. They are conversations in which I worked out how to run a sensor rig. Write me a procedure I could follow next time: the steps in order, the traps I hit and how I got out of them, and anything I decided against and why. Quote the session id from the filename next to each rule you take from it, so I can go back and read the conversation.

That last sentence matters more than it looks. Every session has an id, and the id is enough to reach the conversation again from any machine that can read the archive, without exporting anything: `chat-stasher read --destination <name> --machine <partition> --session <the whole id>` prints it, and its SHA-256, on the spot. A skill file whose rules carry those ids is a document you can check rather than trust.

**What the agent must not do is edit the export.** Treat the directory as read-only input: it is a copy of the archive, and anything the archive keeps should be added to the archive, not to the copy.

## 6. Keep the result

What you keep is the written procedure: a file in your own notes, a skill, an `.md` in a repository, whatever your tools read. The export directory is scaffolding. Delete it when you are done, run `index clear` if you do not want the plaintext index sitting in your cache, and export again the next time you want it. Nothing in the archive changed, so the same command produces the same files, and the manifest will say the same thing.

If a rule in the result turns out to be wrong, you can go back to the conversation that produced it, because the agent quoted its id. That round trip is the point of doing this from an archive rather than from memory.

## What is not possible yet

- **There is no redaction.** Anything exported carries whatever the conversation carried. If a session holds credentials or names you would rather not hand to an agent, do not select it.
- **The CLI counts text matches; the dashboard names them.** `search --text` prints how many matched and how much of the archive it looked at, and stops there. The dashboard's search page is where the list is.
- **`export` cannot select by text.** Machine, tool, id and date only. The text query decides whether the material is worth exporting, not which files get written.
- **Nothing writes a session back into its tool's own folder.** `export` writes files in the tool's format, and putting them back is something the tool does not do.

## Next

- **The machine these sessions came from no longer exists.** [Recover a lost machine's conversations](lost-machine.md) is that situation end to end, including what an exit `3` means and how to read one conversation back.
- **What the local index is and is not.** [how-it-works.md](../how-it-works.md#local-helpers-caches-and-the-index) lists everything chat-stasher keeps on this machine that is not part of the archive, and which of it is plaintext.
- **The command reference.** [cli.md](../cli.md#search) has every flag and exit code for `search`, `export`, `read` and `index`.
