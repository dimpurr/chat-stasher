# Command reference

<!-- RELEASE GATE: written against main. Merged after 0.5.0-rc.2: `index`, `search --text` / `--scan`, `ui --destination a,b|all`, `ui --view extensions`, `overview --json --summary`, the `local` section of `status --json`, `prune-orphans`, and the setup wizard's destination and scheduler steps. -->

Every `chat-stasher` command, its purpose, its main flags and its exit codes. This page is a map. The authority for your installed version is always:

```sh
chat-stasher --help
chat-stasher <command> --help
```

## Commands at a glance

| Command | What it does | Changes anything? |
|---|---|---|
| [`init`](#init) | Writes a commented config file, if none exists | Config (new file only) |
| [`setup`](#setup) | First-run wizard: first archive, key, destination, timer | Yes |
| [`doctor`](#doctor) | Is anything on this machine deleting its history? | No |
| [`run-once`](#run-once) | One archive pass: collect, then push if changed | Stage, archive |
| [`schedule`](#schedule) | Renders, installs or removes the hourly timer | Timer files (with `install` / `uninstall`) |
| [`status`](#status) | Is the timer working? What does the scanner find? | No |
| [`dest-init`](#dest-init) | Seeds a new destination with this machine's history | Stage, destination |
| [`verify`](#verify) | Proves an archive is intact | No |
| [`repair-duplicates`](#repair-duplicates) | Reports identical shard copies without changing the archive | No |
| [`ui`](#ui) | Opens the local dashboard | No |
| [`search`](#search) | Finds sessions by machine, tool, date or text | No |
| [`export`](#export) | Writes selected sessions out as files | Files in `--out` |
| [`read`](#read) | Prints one session | No |
| [`overview`](#overview) | Machine × tool activity matrix, in the terminal or as JSON | No |
| [`index`](#index) | Builds, checks or deletes the local full-text index | Local index |
| [`cache`](#cache) | Shows or clears the local body cache | Local cache (`clear`) |
| [`reclaim-stage`](#reclaim-stage) | Removes staged copies every destination proves it holds | Stage (with `--apply`) |
| [`prune-orphans`](#prune-orphans) | Reports the archive packs no index file names | No |
| [`install-native-host`](#install-native-host) | Registers the browser host | Browser manifests, config |
| [`ingest`](#ingest) | Archives an extension export file | Stage |

Lower-level commands, used by the ones above or for repair, are listed under [Plumbing](#plumbing).

## Conventions

### Flags most commands share

| Flag | Meaning |
|---|---|
| `--stage <dir>` | The stage folder. Never created for you by the browser host. |
| `--destination <name>` | A `[destinations.<name>]` from the config. **Required once any destination is declared**; there is no default. `ui` is the one exception, see below. |
| `--repo <path>` | Use this archive directly, instead of a named destination. |
| `--key-file <path>` | The key file for `--repo`. |
| `--option key=value` | A backend option, repeatable. Never put credentials here: command lines end up in logs and shell history. |
| `--connections <n>` | Concurrency cap. Default 4, maximum 10. |
| `--machine <id>` | The machine partition. Default: `machine` in the config, else this machine's identity. |
| `--keep-ssh-masters` | Leave SSH connection masters open after the command. |
| `--json` | One JSON object on stdout, and nothing else there. |

### Exit codes

The same four codes are used everywhere a command answers a question about the archive:

| Code | Meaning |
|---|---|
| `0` | Answered completely. |
| `1` | Read everything, and the answer is "nothing" (for `status`: not healthy). |
| `2` | Usage error. Nothing was done. |
| `3` | Could not finish reading. Any "0" in the output is unproven. |

Exceptions are listed per command. `ui` never exits `1`.

### JSON output

`doctor`, `status`, `setup`, `overview`, `search` and `prune-orphans` accept `--json`. A value that could not be measured is written as `{"kind":"unknown","why":"…"}`, never as `0`, `null` or a missing field. Known values are `{"kind":"known",…}`, and values that do not apply are `{"kind":"not_applicable",…}`.

### Where output goes

| Command | Report on |
|---|---|
| `status` | **stderr**. `status | head` hides the exit code. `--json` puts the object on stdout. |
| `ui` | stdout: the URL. A closed pipe does not stop the dashboard. |
| ssh-master teardown (`[reap] …`) | **stderr**, on success and on failure alike. It is housekeeping the run did for itself, not the command's answer; a `--json` command must stay parseable. |
| everything else | stdout, with errors on stderr |

## Getting started

### `init`

Writes `~/.config/chat-stasher/config.toml`, with every setting explained in comments. Does nothing if the file exists. See [config.md](config.md).

### `setup`

The first-run wizard: [setup.md](setup.md).

| Flag | Meaning |
|---|---|
| `--stage <dir>` | Stage folder. Required when not interactive. |
| `--masterkey-saved-elsewhere` | The person declares they have a copy of **every key the run names that is already on this machine** — the local archive's and each destination's. Not verified. A key this run creates is not covered: it did not exist when the flag was given, so the run stops with exit `2` and names the file instead. |
| `--destination <name>` | Configure or verify this destination. Omit to skip the remote step. |
| `--remote sftp\|s3` | Which recipe to write for a new destination. |
| `--remote-endpoint`, `--remote-user`, `--remote-ssh-key`, `--remote-bucket`, `--remote-region`, `--remote-root` | The destination's values. `--remote-region` defaults to `auto`. |
| `--remote-access-key-id-env <VAR>`, `--remote-secret-key-env <VAR>` | The **names** of the variables that hold the S3 credentials. |
| `--trust-host` | The person declares they checked the SFTP fingerprint. |
| `--install-schedule` / `--uninstall-schedule` | Install or remove the timers after setup. |
| `--json` | One JSON object. Always on when not attached to a terminal. |

Exit codes: `0` done · `1` a step did not finish · `2` missing parameter or malformed flag, refused before anything was written · `3` something could not be read. One `2` is different: when the declaration is the only thing owed and the run refused *before* the local archive pass (`steps.local_save` is `not_attempted`), it creates the local repository and every key it will ask about, so there are files to copy, and stops there. The same rule reaches a destination's key one step later — any key a run creates cannot be covered by a flag given before it existed, so a run that created one names the new file and stops with `2` as well, whether or not it stopped before the archive pass. [setup.md](setup.md#for-scripts-and-agents) has the details.

### `doctor`

Read-only. Reports each AI tool on this machine, whether its settings delete old sessions, each declared destination (reached or not, with the reason), identical duplicate shards in the configured local stage, the browser host registration, local cache sizes, and the key files this machine holds — one per archive copy, with whether each is present here and whether you have declared a copy of it. Prints paths, counts, sizes and dates, never conversation text. To inspect archived destinations for duplicates, run [`repair-duplicates`](#repair-duplicates).

| Flag | Meaning |
|---|---|
| `--json` | One JSON object. |

## Archiving

### `run-once`

One pass: collect new session data into the stage, then push to the archive if anything changed.

| Flag | Meaning |
|---|---|
| `--stage <dir>` | Required. |
| `--destination <name>` | Required once destinations are declared. |
| `--verify` | Run the cheap structure check (`verify --level l1`) afterwards. |
| `--shard-bucket-cap <n>` | Sealed shards per bucket. Default 20. |

Ends with `result: COMPLETED` (a snapshot was created) or `result: NOOP` (nothing changed). Both exit `0`. Non-zero is a real error. Safe to run again at any time.

### `schedule`

Renders the hourly timer, or installs and removes it. See [schedule.md](schedule.md).

| Form | Effect |
|---|---|
| `schedule --stage <dir>` | Prints the timer. Changes nothing. |
| `schedule --stage <dir> --output <path>` | Writes the timer file and prints the command that loads it. Loads nothing. |
| `schedule install --stage <dir>` | Writes and loads the timer. One per declared destination, unless `--destination` narrows it. A failed install puts the machine back: it stops the timers it had armed, removes or restores the files it wrote, and loads again the agents it had unloaded. On a machine that had no timer, `status` then reports `not_installed`, never an installed timer that is armed nowhere. |
| `schedule uninstall` | Stops and removes the timers for the declared (or named) destinations. |
| `schedule …` on Windows | Refuses with exit `2` and changes nothing: this build has no scheduler integration there. The manual Task Scheduler steps, with a copy-pasteable `schtasks` command, are in [schedule.md](schedule.md#windows-a-task-in-task-scheduler). |

| Flag | Meaning |
|---|---|
| `--format launchd\|systemd` | Default `launchd`. Pass `systemd` on Linux. |
| `--unit run-once\|reclaim-stage` | The hourly archive (default), or the weekly stage clean-up. |
| `--destination <name>` | Repeatable. Not valid with `--unit reclaim-stage`. |
| `--binary <path>` | The binary the timer runs. Paths under `target/` are refused. |
| `--verify` | Add the structure check after each pass. |

The interval is `backup_interval_secs` from the config.

### `status`

Is the scheduled archive working? The first line is the verdict, read from the record the last pass left:

| Verdict | Exit |
|---|---|
| `Healthy: last run … ago, took … ms, archived … shard(s), …` | `0` |
| `Last run failed: the <step> step errored …` | `1` |
| `No run has ever been recorded` | `1` |
| `No run for … (threshold …)`: nothing for more than 4 intervals, and at least 1 hour | `1` |

`3` means the scan itself did not complete.

A `[keys]` line is added for each **destination** whose key needs attention: one that is not on this machine, or one that is here with no declared backup. Each archive copy has its own key and another machine reads it with that one, so an off-site copy whose key nobody backed up is a copy that a lost machine loses. A machine whose destination keys are all present and declared prints no such line, and the local key is never one of them — it is the wizard's step and `doctor`'s first key row.

| Flag | Meaning |
|---|---|
| `--sessions` | Add one line per session found (tool, size, date, short id). Can be hundreds of lines. |
| `--json` | One JSON object, including a `local` section: whether the timer is installed (`installed` needs the units present *and*, on Linux, systemd confirming each timer active — otherwise `unconfirmed`), the next run and why, and the sessions still staged and waiting to upload. The last pass is its own field, `run_state`. A `keys` section carries the full inventory — the local key and every destination's, with each file's path, whether it is on this machine, and whether you have declared a copy. |
| `--destination <name>` | Also list the chat-stasher version each machine last archived with, and flag machines behind the newest. |

### `dest-init`

Seeds a new destination, once. It re-collects this machine's sessions from their sources, copies in what other destinations still hold for this machine that the machine no longer has, and pushes the result. It covers **this machine's** part of the archive only. See [destinations.md](destinations.md).

Re-running it is safe: when the destination already holds a snapshot equal to what would be published, nothing is published and the run says so (`push skipped`). Set `push_only_if_changed = false` to write a snapshot on every run instead. This matters because `setup` runs `dest-init` each time it is invoked.

| Flag | Meaning |
|---|---|
| `--destination <name>`, `--stage <dir>` | Required (or `--repo` instead of a name). |
| `--from <name>` | Compare against only these destinations. Repeatable. Default: every other declared one. |
| `--trust-host` | Record an SFTP server's key in `~/.ssh/known_hosts`, after printing its fingerprint. Never implied. |

A source destination that cannot be read makes the result incomplete, reported as such, with a non-zero exit. A new SFTP host stops the command with exit `3` until you pass `--trust-host`.

### `verify`

| `--level` | Checks | Cost |
|---|---|---|
| `l1` | Archive structure | Cheap. Reads no conversation data. |
| `l2` | Content: downloads and re-hashes every pack | Downloads the whole archive |
| `l3` | Every staged session against the archive: shard count, bytes, SHA-256 | Needs `--stage` |
| `all` (default) | All three | |

L3 also names any session whose archived shard sequence repeats shards byte for
byte. The three checks above cannot see that — a body stored twice is
self-consistent — and it is what leaves older versions' `read` and `export`
returning a conversation twice. It is reported as a **possible** duplicate seal
and does not fail the run. The count is carried in the `L3 verdict` line so a
green run cannot hide it. Fixed versions collapse repeated same-session shard
hashes in readers, but keep the stored shards and snapshots unchanged. The
collapse is a read decision, never a deletion, and it is reversible per run:
`read` and `export` take `--no-collapse`, and the index builders honour
`CHAT_STASHER_NO_COLLAPSE` (see [`read`](#read), [`export`](#export) and
[`index`](#index)).

Each report also names the shape, because the shapes are not equally suspicious.
The repeat is either the whole body sealed before it — the shape a re-seal
leaves, whether that body took one shard or several — or a block that recurs
without beginning the sequence, which is the weakest of the three and the one
least distinguishable from content that genuinely repeats.

### `repair-duplicates`

Read-only dry-run inventory of identical shard content in the newest archived
body for each session and machine partition:

```sh
chat-stasher repair-duplicates --destination <name>
chat-stasher repair-duplicates --destination <name> --json
```

The report gives duplicate session, shard, and byte counts per machine. A later
run is a duplicate only when it is a byte-identical replay, shard by shard, of
the session's complete preceding shard sequence. Individual shard repeats
outside such a replay, including `A,B,A`, are reported separately as suspicious
and kept. A shard whose bytes equal the concatenation of earlier shards is kept
unless it also matches the corresponding preceding shard individually.

The report also **lists every collapsed run**: each session a read would
collapse is named with the run's start position in the shard sequence, how many
shards it spans, the bytes those shards hold, and the session's total shard
count, so a run that spans the whole sequence (the re-seal shape) reads apart
from a replay inside longer content. In `--json` these are `collapsed_runs`; the
text report prints one `collapsed-run` line per run. Sessions are labelled with
the same privacy-safe short id `read` prints, never the raw id.

For example, `chat-stasher repair-duplicates --destination backup --json`
reports the runs readers would collapse without changing the destination.

`--json` prints one object with `complete`, per-machine duplicate counts,
`collapsed_runs`, `suspicious_kept_*` counts, and `dry_run: true`. Exit `0` means
the archive was fully read, `3` means it was not fully read, `2` means the
command or destination was invalid, and `1` means reading completed but report
generation failed.

This command never deletes shards or snapshots and never runs `forget`, `prune`,
or `rewrite`. Physical removal is a separate decision; the append-only policy
keeps the original data in place.

### `reclaim-stage`

Removes staged copies, but only for sessions that **every** declared destination proves it holds, byte for byte. An unreachable destination blocks it.

| Flag | Meaning |
|---|---|
| `--stage <dir>` | Required. |
| `--apply` | Actually remove. Without it, a dry run that removes nothing. |

Exit codes: `0` reclaimable or reclaimed · `1` blocked, nothing removed · `2` usage error.

### `prune-orphans`

Reports the packs a destination's backend holds that **no index file names** —
what a push killed between writing its packs and its index leaves behind — and
says what the next push would do about each one. It **deletes nothing**: dry run
is the only mode in this build.

`--apply` is refused with exit `3`, naming what a safe delete would need and this
build does not have: a repository-wide lock every client honors for its whole push
or read, a conditional delete carrying an object version, and a trustworthy
backend modification time to age candidates by. The refusal is not a formality.
"Unindexed" means no index file names the pack, not that nothing uses it — a
retry is allowed to reuse one and write a snapshot that depends on it while it
stays unindexed on disk, so a pack can be the only copy of archived content.
[docs-dev/orphan-packs.md](../docs-dev/orphan-packs.md) has the full argument.

Every pack it reports is verified the way the adopting open verifies one before
reusing it: the header must decrypt, every blob it declares must decrypt and hash
to the id the header gives, and the pack's bytes must hash to the id it is stored
under.

| Flag | Meaning |
|---|---|
| `--destination <name>` | Required (or `--repo`). One destination per run, always named. |
| `--dry-run` | The default, and the only mode. |
| `--apply` | Refused, exit `3`, naming the missing capabilities. |
| `--json` | One object, carrying each candidate pack's full id for audit. |

The report carries the repository's **own id** from its config — the same on every
machine that reads it — and never the path, host or account it was reached at. Along
with it: the backend family, pack and byte totals, the unindexed packs by id prefix
and size, how many of those verified, how many are unknown with the verifier's own
reason, and any pack an index names that the backend no longer lists. No
conversation text appears in it.

Exit codes: `0` the survey read every pack · `1` it read everything and found an
index naming a pack the backend does not list · `3` it did not finish reading — a
pack or an index file could not be read, the backend was unreachable, or `--apply`
was asked for — so no absence in the output proves anything · `2` usage error.

An empty answer and an unknown one never look alike: a pack that could not be read
is reported `unknown` with its size and the verifier's reason, never as nothing
there, and it makes the whole survey incomplete. A clean report of zero candidates
is produced only after a complete survey.

Nothing calls this command for you: no schedule, no `push`, no `run-once`, no
browser host.

## Getting your conversations back

### `ui`

A dashboard on `127.0.0.1`, on a port the system picks, with a random token in the URL. It serves GET requests only and closes after 5 idle minutes. Pages: an overview (totals, machine × source matrix, weekly heatmap, drill-down), session lists, a reader that decrypts one session on request, `/search` over the [local index](#index), a download of one session as a file, and **Extensions** (one row per browser install, grouped by machine).

| Flag | Meaning |
|---|---|
| `--destination <names>` | One name, several (`a,b`), or `all`. Several are merged into one view: a session held by more than one destination is listed once, with a badge, and the first destination named wins where copies differ. |
| `--view overview\|extensions` | The page to open first. |
| `--session`, `--machine`, `--harness`, `--day`, `--since`, `--until` | Start with a filter applied. Same meaning as in `search`. |
| `--no-open` | Print the URL without opening a browser. |
| `--idle-timeout <s>` | Default 300. `0` never idles out. |

**Which destination it opens.** An explicit `--destination` or `--repo` always wins. Otherwise: the only declared destination, if there is one; with several, the one set as `destination` under `[native_host]`; with several and none set, it lists them and exits `2`.

`--repo`, `--key-file` and `--option` are refused with more than one destination.

Exit codes: `0` served, including an honest empty page for an empty archive · `3` could not read the archive in full (or no key) · `2` usage error. Never `1`.

`view` is a deprecated alias for `ui`.

### `search`

Finds sessions in **one** destination, always named.

| Flag | Meaning |
|---|---|
| `--destination <name>` | Required (or `--repo`). |
| `--session <prefix>` | Session id prefix. |
| `--machine <id>` | One machine partition. |
| `--harness <ids>` | Comma-separated tool ids, for example `claude-code,codex`. |
| `--day`, `--since`, `--until` | Local calendar days, `YYYY-MM-DD`. Filters on the **conversation's own dates**, not on when it was backed up. |
| `--text <query>` | Search conversation text in the [local index](#index). Three characters or more. |
| `--scan` | With `--text`: read the selected conversations and match case-insensitively instead. Answers short queries, and downloads what it reads. |
| `--cost` | Also report what reading the selected sessions in full would cost. |
| `--json` | One object: matched, not matched, and could-not-be-placed groups. Text searches carry the index's coverage and a `query_state` for the query. |

A `--text` query shorter than the three characters the index's trigram tokenizer
matches cannot be evaluated at all, so it is not a search that found nothing: the
run exits `3` — the same "this proves nothing" family as an unreadable snapshot —
the report prints `matched=unknown` where a count would otherwise be, and the
JSON says `query_state: "too_short"` beside `query_length` and `query_minimum`,
and `matched` is `null` rather than a count — so a caller that reads `matched` as
an integer fails here instead of being handed a `0` that was never measured.
`--scan` answers such a query by matching the conversations themselves.

Without `--text`, search reads metadata only and never downloads a conversation. It walks **every snapshot** of a machine — not just the newest one — so a session whose local bodies have been reclaimed (`reclaim-stage` deletes them once every destination has proved it holds them) is still found, reported against the snapshot that actually holds it. The run says `snapshots scanned: N of M`, and names the shortfall when there is one (`2 of 3, 1 unreadable`); a snapshot that could not be walked is a set of sessions that was never looked for, so `not in this destination`, exit `1`, is printed only when all M were walked, and anything less is exit `3` — an unknown, never a negative.

A session whose dates are unknown is listed separately, never dropped. While any remain and a date filter is active, "0 matched" exits `3`, not `1`.

`--session` matches from the **start** of the stored session id, which begins with the tool (`claude-code.<machine>.<native id>`). A native id on its own therefore matches nothing, and the run says "not in this destination" without that being a fact about the archive. [Recover a lost machine's conversations](use-cases/lost-machine.md#5-read-one-back) has the two forms side by side.

### `export`

Writes exactly the sessions `search` selects for the same flags, as `<out>/<machine>/<tool>/<session-id>.jsonl` in each tool's own format, plus a checksummed `<out>/manifest.json`.

| Flag | Meaning |
|---|---|
| `--out <dir>` | Required. Must be empty or absent, unless `--force`. Nothing is ever deleted. |
| filters | As in `search`. |
| `--turns all\|user` | `user` keeps only the person's own messages for Claude Code. Other tools' formats do not prove which lines are the person's, so every line is written and the manifest records `turns_filter: "not-supported"`. |
| `--trim-to-window` | With a date filter, also drop lines timestamped outside it. |
| `--no-collapse` | Write every stored shard. By default a run that byte-for-byte replays the session's complete preceding shard sequence is collapsed to its first copy, because that is what a re-seal leaves. |
| `--dry-run` | Print the plan and its cost. Writes nothing. |

Exit codes: `0` wrote sessions and answered for everything · `1` selected nothing · `3` incomplete: what was written is real, and the manifest lists what is missing · `2` usage error. `--dry-run` reports the same exit code the real run would, so it is worth running first.

`--turns user` currently applies to Claude Code only. It keeps messages the archived
format identifies as the person's own: qualifying `type: "user"` records and
`attachment` / `queued_command` records typed mid-turn. Tool results, metadata,
sidechain messages, compaction summaries, and injected notices are excluded.
When `origin.kind` is present, only `human` qualifies; older records without
that field use the injected-notice prefix check. Matching mid-turn and turn
copies of the same message within fifteen minutes are written once. The message
text is not edited. For every other tool, `--turns user` keeps every line and
records `turns_filter: "not-supported"` in that session's manifest entry; it
does not guess which lines are human. For example:

```sh
chat-stasher export --destination backup --out ./exported --harness claude-code --turns user
```

There are no text filters here: the flags are `search`'s, minus `--text`. Use a text search to decide whether the material is worth exporting, then select with machine, tool and dates. [Turn old conversations into a skill](use-cases/skill-from-history.md) is that workflow end to end.

### `read`

Prints one session (`--session <id>`) and its SHA-256, without content: the shard list in sequence order, the concatenated length and digest, and — only when you pass `--stage <dir>` — the digest of the shards on your own disk, to compare against. `--stage` is a comparison aid, never a requirement and never the addressing scheme: the session is resolved from the archive by `--machine` and `--session`, out of the newest snapshot that holds its shards. So a session whose local bodies were reclaimed (see `reclaim-stage`) still reads back, and you do not need the archiving machine's stage path to read its conversations. A snapshot **newer** than the one that holds the copy, which cannot be read, makes the read exit `3` with that snapshot named: an older copy is never returned as the session's current bytes.

`--no-collapse` prints every stored shard instead of joining the copies a whole-content replay repeats. By default a run that byte-for-byte replays the session's complete preceding shard sequence is collapsed to its first copy — the shape `dest-init` and older setups left, and one no reader can tell from new content that happens to repeat the whole session. Nothing is deleted by either choice: the extra shards stay in the archive, and `--no-collapse` is how you see exactly what a collapse dropped. The same flag applies to `--all-machines`.

`--all-machines` instead reports, across **every snapshot of every machine** cumulatively, each session's id, shard count, length and digest — a session is listed against the newest snapshot that holds it. It reads a lot: unlike `search` it downloads and hashes every shard it lists, so it is a full read of the destination, not a listing. If you only need the enumeration — which sessions exist, on which machine, held where — `search` does the same walk (tree metadata only, no `--text`) and never downloads a shard's payload.

Exit codes: `0` read · `1` completed and the result failed · `3` did not finish reading (no key, repository unreadable, session not in any snapshot it could read) · `2` usage error.

`--session` here is the **whole** stored id, `<tool>.<machine>.<native id>`, and unlike `search --session` it accepts no prefix: anything shorter exits `3` with "holds no shards in any of the snapshots of machine …", which means it could not be resolved rather than that it is absent. The full ids are in an `export`'s `manifest.json`, and `read --all-machines --full-ids` prints them all at the cost of reading and hashing every shard. [Recover a lost machine's conversations](use-cases/lost-machine.md#5-read-one-back) is the worked version.

### `overview`

The machine × tool activity matrix and heatmap for one destination, in the terminal. A machine with no activity index is listed as "index missing", never dropped.

| Flag | Meaning |
|---|---|
| `--destination <name>` | Required (or `--repo`). |
| `--width <n>` | Terminal width. Default 100. |
| `--json` | The full document. |
| `--json --summary` | Totals, one record per machine and per tool, and the last 30 local days. No per-session list. |

Exit codes: `0` rendered · `1` read in full, no activity index anywhere · `3` could not read in full · `2` usage error.

### `index`

The local full-text index behind `/search` and `search --text`. It is **plaintext**, kept in this machine's cache folder, one per destination, and never uploaded. See [privacy-security.md](privacy-security.md).

| Subcommand | Effect |
|---|---|
| `index build --destination <name>` | Reads sessions that changed since the last build and updates the index. A session that cannot be read is named as not indexable with its reason and the rest are still indexed, so one malformed shard does not put the whole archive out of reach. A session whose re-read fails keeps the text the earlier build stored, but stops counting as covered: `index check` reports `partial`, and `search` says the index is behind rather than answering "no match" for text it never read. |
| `index check --destination <name>` | Validates the index, counts what it holds and reports the last build's outcome, without contacting the archive. `state=valid` means a build finished with every read session indexed; `state=partial` names how many were not indexable; `state=incomplete` means no build has finished, so the index answers nothing. |
| `index clear --destination <name>` | Deletes that destination's index. |

`index build` reads each archived session in the format its harness archives —
one JSONL record per line for `claude-code`, `codex` and `kimi-code`, the
pretty-printed document for `gemini-cli`, the exported SQLite row for
`opencode`, `cursor`, `grok` and the rest, a markdown transcript for `aider`.
Two states are kept apart, and `index check` and `search --text` both report
the second one rather than folding it into the first:

- a session whose archived format this build **cannot read** is counted as
  `not indexable: <format>` (for example `not indexable: sqlite`), never as
  indexed, and a query over a view holding one exits `3` — it was not read, so
  a zero says nothing about it;
- a session whose format *was* read and holds no conversation is an empty body,
  which is a measurement.

`search --text` also finds a session by its own id, or by any prefix of it, even
though an id is not conversation text.

An index has no `--no-collapse` flag; like `read` and `export` it collapses a
run that byte-for-byte replays a session's complete preceding shard sequence, and
setting `CHAT_STASHER_NO_COLLAPSE` to any value other than `0`, `false`, `no`,
`off` or empty makes the next build index every stored shard instead. This
covers the full-text index here and the activity index (`activity-index`,
`overview`), which share the same reader. Clearing and rebuilding the index after
changing the variable is what makes the difference visible.

### `cache`

The body cache: encrypted copies of conversations you have opened, so the next read is fast. Nothing in it is decrypted, and deleting it only costs speed.

| Form | Effect |
|---|---|
| `cache` | Prints the location, the quota and what it holds. |
| `cache clear` | Deletes every cached entry. Never touches an archive, a destination or a key. |

## Browser extension

### `install-native-host`

Registers this binary as the host the extension delivers to, for every browser found on this machine (or those named). One registration serves every profile of each browser.

| Flag | Meaning |
|---|---|
| `--stage <dir>` | The stage the host writes to. Must exist. Recorded as `[native_host] stage`. |
| `--browser <name>` | Repeatable: `chrome`, `chromium`, `edge`, `brave`, `arc`, `chrome-beta`, `chrome-canary`, `opera`, `vivaldi`, `firefox`. Default: every browser whose data folder exists. |
| `--uninstall` | Removes exactly the files this command writes, for every browser (or those named). |

Prints every path it writes, skips or removes. Running it twice is harmless.

Exit codes: `0` at least one registration in place (or removal finished) · `3` no known browser found, nothing written · `1` an action failed · `2` usage error.

### `ingest`

Archives extension export files, the ones the popup saves when you export undelivered captures, into the stage. The same bytes are never archived twice.

| Flag | Meaning |
|---|---|
| `--inbox <dir>` | The folder holding the export files. Processed files move to `<inbox>/consumed/`. |
| `--stage <dir>` | The stage. |

## Plumbing

These run inside `run-once`, `dest-init` and the browser host, or repair specific things. You rarely need them directly.

| Command | Purpose |
|---|---|
| `collect` | Reads new session data from each tool into the stage. Exits `3` when a tool has sessions this build cannot archive. |
| `push` | Moves sealed stage data into the archive. |
| `seal` | Seals one file already inside the stage. Refuses any tool whose files are unsafe to rename. |
| `activity-index` | Rebuilds the per-machine activity index. `--rebuild --destination <name> --machine <name>` rebuilds it from the archive — all a machine whose stage is gone still needs. Your own machine's partition is repaired in the archive by appending a new snapshot when `--stage` names a directory to restore the shards into; any other machine's — and your own with no such stage — is rebuilt read-only into a local derived index and the archive is left untouched (a partition's index is written only by the machine that owns it). |
| `machine-declare` | Records this machine's display name (default: its host name). |
| `machine-label` | Names a machine that can no longer name itself, such as a sold laptop. |
| `native-host` | The host process the browser starts. `--self-test` prints one line of JSON and exits. |
