# Command reference

<!-- RELEASE GATE: written against main. Merged after 0.5.0-rc.2: `index`, `search --text` / `--scan`, `ui --destination a,b|all`, `ui --view extensions`, `overview --json --summary`, the `local` section of `status --json`, `prune-orphans`, the setup wizard's destination and scheduler steps, and `hide` / `unhide` / `purge` (dry-run preview) / `redact` (refused surface). -->

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
| [`run-once`](#run-once) | One archive pass: pull any declared producer, collect, then push if changed | Stage, archive |
| [`schedule`](#schedule) | Renders, installs or removes the hourly timer | Timer files (with `install` / `uninstall`) |
| [`status`](#status) | Is the timer working? What does the scanner find? | No |
| [`dest-init`](#dest-init) | Seeds a new destination with this machine's history | Stage, destination |
| [`verify`](#verify) | Proves an archive is intact | No |
| [`repair-duplicates`](#repair-duplicates) | Reports identical shard copies without changing the archive | No |
| [`ui`](#ui) | Opens the local dashboard | No |
| [`search`](#search) | Finds sessions by machine, tool, date or text | No |
| [`export`](#export) | Writes selected sessions out as files | Files in `--out` |
| [`read`](#read) | Prints one session | No |
| [`hide`](#hide--unhide) | Hides one archived session from readers, reversibly | Stage control record |
| [`unhide`](#hide--unhide) | Restores a hidden session | Stage control record |
| [`purge`](#purge) | Previews deleting one named session from selected destinations; the deletion itself is not implemented | No |
| [`redact`](#redact) | Not implemented; refused with the phase boundary | No |
| [`overview`](#overview) | Machine × tool activity matrix, in the terminal or as JSON | No |
| [`index`](#index) | Builds, checks or deletes the local full-text index | Local index |
| [`cache`](#cache) | Shows or clears the local body cache | Local cache (`clear`) |
| [`reclaim-stage`](#reclaim-stage) | Removes staged copies every destination proves it holds | Stage (with `--apply`) |
| [`prune-orphans`](#prune-orphans) | Reports the archive packs no index file names | No |
| [`mirror`](#mirror) | Keeps a byte-exact local copy of one declared destination's repository | Mirror copy under the data directory (see [config.md](config.md#mirror)) |
| [`install-native-host`](#install-native-host) | Registers the browser host | Browser manifests, config |
| [`ingest`](#ingest) | Archives an extension export file | Stage |
| [`import`](#import) | Turns an official export (a takeout) into inbox bundles, or feeds a copied Grok Bot cache backup in as an archive pass | Takeout mode: an existing `--stage` and a named `--inbox`. Backup mode: `--stage` and one or more `--from` directories, read-only |
| [`backfill`](#backfill) | Recovers a Grok Bot conversation's pre-window history by letting its own app load it (opt-in, macOS only) | App: quits and relaunches Grok Bot; Stage, archive (only with `--i-accept-the-risk`) |

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

`doctor`, `status`, `setup`, `overview`, `search`, `prune-orphans` and `purge` accept `--json`. A value that could not be measured is written as `{"kind":"unknown","why":"…"}`, never as `0`, `null` or a missing field. Known values are `{"kind":"known",…}`, and values that do not apply are `{"kind":"not_applicable",…}`.

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

Read-only. Reports each AI tool on this machine, whether its settings delete old sessions, each declared destination (reached or not, with the reason), identical duplicate shards in the configured local stage, the browser host registration, local cache sizes, and the key files this machine holds — one per archive copy, with whether each is present here and whether you have declared a copy of it — and how the declared set agrees with what the timers and the weekly stage clean-up actually do: push coverage, credentials a timer can resolve, the last recorded push per destination, and the latest recorded reclaim. Prints paths, counts, sizes and dates, never conversation text. To inspect archived destinations for duplicates, run [`repair-duplicates`](#repair-duplicates).

Every check runs behind its own deadline, and each prints a `doctor: checking …` line before it starts, so a wedged step names itself instead of holding the whole report hostage: a check that does not answer within its budget is abandoned, its section is worded as unknown (never zero, never absent), the sections that did answer still print, and the run ends with exit `3` — the same "could not look" code an unreadable config gets, never a clean-looking `0` for a machine only half of which was inspected. A destination that accepts the connection and then says nothing is the case this exists for: its line reads `NO ANSWER WITHIN <N>s`, which is unknown, not a refusal. A timed-out section keeps its own heading and one `UNKNOWN` line naming the check and the budget, so a hole in the report is a named hole, not a missing section. The config is read once, behind the same deadline, and every later step that needs it reuses that one read — a config file that never answers gets one bounded attempt, not a second unbounded one. On a slow link, budgets are what a healthy probe fits in with room to spare — a check that needs longer than its budget on a healthy machine is a bug in the check, not a budget to raise; say so in an issue.

The run is bounded because every check is: the worst case is the sum of the per-check budgets — roughly 25 minutes with two destinations declared (240s each for the scan and the reclaim plan, 90s per file walk behind the cache, keys, native host, reconciliation and mirror sections, 45s to connect to a destination and 120s to read one, 30s for the settings/config/inbox reads), not an open-ended wait. A healthy machine finishes in seconds; the sum is what a wedged-everything run is capped at.

`CHAT_STASHER_DOCTOR_CHECK_BUDGET_SECS=<seconds>` replaces **every** check's budget with one number (values below 1 are ignored, and an unset or unparsable value keeps the per-check defaults). It is the product override for the two cases the default budgets do not fit: a healthy-but-slow machine whose probe legitimately needs longer than its budget, and an operator who wants a faster or stricter bound. Changing it changes what a timeout means — a probe cut off at 1s says far less than one cut off at 45s — so prefer the per-check defaults when reporting an issue.

| Flag | Meaning |
|---|---|
| `--json` | One JSON object. Under `--json`, a non-empty `timed_out` array lists the checks that outlived their budget (`check`, `budget_secs`); the sections it names carry placeholders and must be read as "did not look", exactly as under a non-null `config_error` — each such section answers `{"checked": false}` and, when a deadline note names it, also carries `timed_out_after_secs`. |

## Archiving

### `run-once`

One pass: collect new session data into the stage, then push to the archive if anything changed. A declared `[pull.letta]` producer (see [pull.md](pull.md)) is pulled before the collect step, so the pass archives it too; a producer that cannot finish reading keeps the pass at exit 3 while the local cycle still runs.

| Flag | Meaning |
|---|---|
| `--stage <dir>` | Required. |
| `--destination <name>` | Required once destinations are declared. |
| `--verify` | Run the cheap structure check (`verify --level l1`) afterwards. |
| `--shard-bucket-cap <n>` | Sealed shards per bucket. Default 20. |

Ends with `result: COMPLETED` (a snapshot was created) or `result: NOOP` (nothing changed). Both exit `0`. Non-zero is a real error; `3` includes the case where a declared pull producer did not finish reading. Safe to run again at any time.

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
| `--json` | One JSON object, including a `local` section: whether the timer is installed (`installed` needs the units present *and*, on Linux, systemd confirming each timer active — otherwise `unconfirmed`), the next run and why, and the sessions still staged and waiting to upload. The last pass is its own field, `run_state`. A `keys` section carries the full inventory — the local key and every destination's, with each file's path, whether it is on this machine, and whether you have declared a copy. The same section also carries `destinations` (the four facts per destination: installed-timer push coverage, credentials a timer can resolve, the last recorded successful push, and its share of the latest recorded reclaim; plus an `upgrade_window` flag, true when the repository provably holds snapshots while no push is on record — the window between the upgrade and the first recorded push, readable as a field instead of by matching the alert sentence), `dest_alerts` (the same findings as the `[dest]`/`[reclaim]` lines, each with `severity` and fix), `reclaim` (the weekly clean-up's durable record: state, consecutive blocked runs, what is held back and which destination the record names per block, with the block's kind — a `not-mirrored` block names the mirroring destination the copy lags behind, and the mirrored, proven destination is never the one its bytes are worded "held back for" on any surface), and `mirror` (the local copy's field family when `[mirror]` is configured: size, last sync, snapshots behind and orphans, each measured / unknown / not applicable, plus the already-worded `rows` the menu bar app displays). |
| `--destination <name>` | Also list the chat-stasher version each machine last archived with, and flag machines behind the newest. |

`[dest]` and `[reclaim]` lines are added when a declared destination disagrees with what the machine's timers and the weekly stage clean-up actually do: a destination no installed timer pushes, one whose `env:` credentials a timer cannot resolve, one a timer covers but no push has been recorded against, one whose `push-state` record exists but cannot be read, or a reclaim blocked for several weeks running. Each line is the same sentence `doctor` reports and the menu bar app shows, with the fix command beneath it. A machine whose destinations are all covered, timer-resolvable, record-successful and currently reclaiming prints no such line — and the lines do not change the exit code, which stays the timer verdict documented above.

A `[mirror]` line is added when the machine's local mirror has something to say: a first fill in progress (phrased as progress — `X% (bytes downloaded of the total)`, never as a red), a round that failed, a record that cannot be read, or the steady row once the copy is complete — its size on disk, the last sync's time and age, how many snapshots the closure record does not cover, and how many orphan files the copy carries. Those four are the mirror's settled field family: the same wording shows in this line, in `doctor`'s `D13` section and in the menu bar app, which decodes the rows from `--json` rather than rewording them. No `[mirror]` section configured prints no line at all — while a section with the switch off still prints the one line it owes when a leftover copy is on disk (the copy's measured size and file count; it keeps every byte until you delete it — see [config.md](config.md#mirror)), so turning the mirror off is silence only when no bytes are left behind. A mirror line never moves the exit code either.

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

Removes staged copies, but only for sessions that **every** declared destination proves it holds, byte for byte. An unreachable destination blocks it. On a machine whose `[mirror]` is enabled it is also hold-back gated by the copy: a session is refused — `not-mirrored` — until the mirror's completed-round record holds the proof's snapshot closure-complete, so a completed `mirror sync` round is what frees it (the mirror rows name the count it holds back).

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

Every pack it reports is classified the way the shared interrupted-push
classification does: a pack whose bytes verify complete is **stranded** — a
later open adopts it, and the report says so; a pack that cannot be read as a
pack at all (its length field declares a header the file cannot hold, or the
header will not decrypt) is **torn debris** from an interrupted push — it
belongs to no backup and can be ignored, and it makes the survey neither
incomplete nor a failure; anything else is `unknown` with the verifier's own
reason. `verify`, `doctor`, and `mirror sync` use the same classification and
the same sentence for torn debris — see
[use-cases/interrupted-push.md](use-cases/interrupted-push.md).

| Flag | Meaning |
|---|---|
| `--destination <name>` | Required (or `--repo`). One destination per run, always named. |
| `--dry-run` | The default, and the only mode. |
| `--apply` | Refused, exit `3`, naming the missing capabilities. |
| `--json` | One object, carrying each candidate pack's full id for audit. |

The report carries the repository's **own id** from its config — the same on every
machine that reads it — and never the path, host or account it was reached at. Along
with it: the backend family, pack and byte totals, the unindexed packs by id prefix
and size, how many of those are torn and stranded (each with their own count and
byte total), how many are unknown with the verifier's own reason, and any pack an
index names that the backend no longer lists. The earliest-seen time of a torn or
stranded pack is reported as unknown — the backend interface carries no
modification time, and no field invents a clock. No conversation text appears in
it.

Exit codes: `0` the survey read every pack (torn debris included — it is a known
state, not an unfinished read) · `1` it read everything and found an
index naming a pack the backend does not list · `3` it did not finish reading — a
pack or an index file could not be read, the backend was unreachable, or `--apply`
was asked for — so no absence in the output proves anything · `2` usage error.

An empty answer and an unknown one never look alike: a pack whose header reads
but whose bytes do not verify is reported `unknown` with its size and the
verifier's reason, never as nothing there, and it makes the whole survey
incomplete. Torn debris is not that case: it is established, classified, and
reported as its own count. A clean report of zero candidates
is produced only after a complete survey.

Nothing calls this command for you: no schedule, no `push`, no `run-once`, no
browser host.

### `mirror`

`mirror sync` runs one copy round: list the declared destination's whole
repository, download the files the local mirror lacks, and hash every file
against its own name (`config`, the one non-addressed file, is compared and
copied byte for byte). The result lives under
`~/.local/share/chat-stasher/mirror/<name>/` — the *storage*, the `keys/`
directory, `index/`, `snapshots/` and `data/`, everything except `locks/`,
which this build never holds. Nothing is decrypted to copy it; with
`[mirror]` enabled, the scheduled pass runs the same round after its own
push, inside the pass's leftover budget. Declare one in
[config.md](config.md#mirror).

And the copy is not just for keeping: with the switch on, the mirror also
**serves** that destination's reads — `read`, `search`, `export`, `index
build` and the `ui` dashboard answer bytes from the local copy first and
fall back to the remote per file it lacks, so a fully mirrored machine
reads offline, and each command's report shows which side served what.
What the reads are worth is dated — "the local copy as of HH:MM" — and
`verify` never consults the copy. The read rules (offline, dated answers,
what stays the remote's) are stated in [config.md](config.md#mirror).

One run is one bounded round: a large first fill spans several rounds, each
resuming where the previous stopped, and `status` reports the progress as
progress — a round that stops early is not a failure. Before a first fill
starts, the command estimates whether the remaining download plus the
free-disk floor fits the free space, and refuses with both numbers when it
does not. A mirror file that is present but unreadable is never quietly
re-fetched mid-read: a command reading over it stops with the file's name
and points back at `mirror sync`, whose next round re-downloads a file
that no longer hashes to its name.

What the copy keeps is on every surface: `status` prints one `[mirror]`
line, `doctor` shows a `D13` section, and the menu bar app decodes the
same rows from `status --json` without rewording them — the size on disk,
the last completed sync with its age, how many snapshots the closure
record does not cover, and the orphan files a remote cleanup left behind.
Orphans are never removed automatically: `doctor` reports their
reclaimable bytes, and the manual act is to delete the files by hand or
remove the whole mirror directory and re-run `mirror sync`, which rebuilds
a copy without them.

| Flag | Meaning |
|---|---|
| `--budget-secs <n>` | The round's budget. Default 600. A stop here is a pause; the next round resumes. |
| `--keep-ssh-masters` | Leave SSH connection masters open after this run (they are shut down by default on every exit path). |

Exit codes, the same vocabulary `verify` and `read` use — and the one place
a "checked and failed" answer exists for the *remote*:
`0` the mirror holds everything the remote listed, verified — torn debris from an interrupted push excepted: a half-written pack no index file names is classified, skipped, and counted (`[mirror] torn : …` with the standard sentence), never downloaded, never a failure —
`3` the round did not finish — the budget ran out, the free-disk floor tripped, the space pre-estimate refused to start, or the remote could not be read mid-round — progress is recorded and the next round resumes ·
`1` a finished read that failed: the source proved corrupt (a file hashed against its own name twice and failed both times — a pack an index file names, or one that is neither torn debris nor complete; not a transfer a later round can heal), a local write failed, or the listing named a file over the whole-file bound ·
`2` usage error — no `[mirror]` section, the switch is off, `mirror_of` names no declared destination, the destination has never been initialised (run a push first), or the destination's masterkey file is missing on this machine or unusable. Nothing was read or written.

The round's own records (`mirror-state.json`, written only by a completed
round, and `mirror-progress.json`, written every round) carry ids, counts
and destination names — no repository paths, and no raw error chains.

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
| `--include-deleted` | For snapshot-source sessions (opencode), also write the records the source deleted, at their original positions, each carrying `"deleted": true`. The default view stays the source's final state; deleted history is never erased from the archive. Other harnesses ignore the switch. |
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

Prints one session (`--session <id>`) and its SHA-256, without content: the shard list in sequence order, the concatenated length and digest, and — only when you pass `--stage <dir>` — the digest of the shards on your own disk, to compare against. The archive is addressed by `--machine` and `--session`; `--stage` only adds a comparison. Reads walk the machine's snapshots newest first, keep the newest copy of each shard path, and retain older sequences absent from later snapshots. This preserves history when a push after `reclaim-stage` contains only a new tail. An unreadable snapshot makes the read exit `3`: an incomplete history is never presented as a complete session.

`--no-collapse` reads every stored shard. Default reads collapse legacy whole-content replays, while retaining shards covered by verified sequence-bound capture observations, even when their bytes repeat. A fresh source root's matching prefix alone cannot establish that its records are a replay; provenance-bearing sources are captured conservatively under the same session identity. Nothing is deleted by either policy. Missing provenance retains legacy behavior; malformed provenance makes metadata-dependent default reads fail rather than silently discard a captured repeat. `--no-collapse` remains available for raw body reads and also applies to `--all-machines`.

For snapshot-source sessions (opencode) the reported body is the **assembled** view, not the raw shard concatenation: every whole-file observation and every increment record the archive holds folds by last-version-per-record, deletions the source performed (revert, compaction) leave the view the same way they left the source's own database, and the reported digest is of that view. At quiescence it equals a fresh export of the source's final state; the older multi-copy era reported the concatenation of every copy, which equaled no single version of anything. The raw shard identities printed beside it remain what `verify` L3 proves. A body that cannot be assembled is an exit `3` read failure — never a short or empty session. `--include-deleted` asks for the other view: deleted records are included at their original positions, each marked `"deleted": true`, so the history the source threw away stays reachable (`read` then reports that view's digest; `export --include-deleted` writes the marked lines).

`--all-machines` reports each session's id, shard count, length and digest across **every snapshot of every machine** cumulatively. The machine row identifies its newest snapshot; session bodies also include older retained sequences. This downloads and hashes payloads. For an enumeration without payload downloads, use `search` without `--text`; its snapshot labels identify where the session was most recently observed.

Exit codes: `0` read · `1` completed and the result failed · `3` did not finish reading (no key, repository unreadable, session not in any snapshot it could read) · `2` usage error.

`--session` here is the **whole** stored id, `<tool>.<machine>.<native id>`, and unlike `search --session` it accepts no prefix: anything shorter exits `3` with "holds no shards in any of the snapshots of machine …", which means it could not be resolved rather than that it is absent. The full ids are in an `export`'s `manifest.json`, and `read --all-machines --full-ids` prints them all at the cost of reading and hashing every shard. [Recover a lost machine's conversations](use-cases/lost-machine.md#5-read-one-back) is the worked version.

### `hide` / `unhide`

Hide is presentation state, not deletion: the session's archived bytes are untouched, and every ordinary reader omits the identity while it is in force — `read --all-machines`, `search`, `export`, the UI, `index build` and `read --session` (which refuses with exit `1`, a finished read that the policy refuses, not a `3` that would blame the archive). `unhide` appends a later record and restores visibility. The state is durable control data, not a local preference: it is appended to `meta/<machine>/deletion-control-v1.jsonl` — this machine's own metadata partition (ADR-017), one JSONL record per command — and the next ordinary `push` archives it with the stage, so every machine honors it from the destination it reads.

| Flag | Meaning |
|---|---|
| `--stage <dir>` | The stage whose meta tree receives the record. Required; never created. |
| `--machine <id>` | The recording machine partition, as everywhere; also the target's partition unless `--target-machine` is given. |
| `--target-machine <id>` | The partition the target identity lives under, when it is not the recording machine's own. |
| `--reason <label>` | Optional, at most 80 bytes, one line. Metadata the user chose to keep — never conversation text. |

The identity is exact: the session id as named, in one machine partition. A session with the same id under another partition is a different identity and is never affected, and no id is prefix-matched. Hiding twice appends nothing (the state is already in force); un-hiding a visible session appends nothing. A destination whose control stream is unreadable or cannot be established fails the reads above closed (exit `3`) — an unknown control state is never treated as "nothing recorded".

Exit codes: `0` the state is recorded or already in force · `1` the record could not be appended durably · `2` usage (missing `--stage`, invalid identity, over-long reason) · `3` this machine's identity could not be resolved, or the existing control stream is unreadable — nothing is written over an unknown state.

Run `index build` after the next push so the full-text index reflects the state; the index is built from the same policy every reader applies, but it does not rebuild itself.

### `purge`

Read-only dry-run preview of deleting one named session from explicit destinations ([the design](../docs-dev/named-session-purge.md)). **The default is the dry run, and in this build there is no other mode**: the destructive purge — the exclusive maintenance gate, the tombstone publication barrier, snapshot rewrite, forget and prune — is not implemented. The preview reads every selected destination and reports, per destination, whether the exact named identity is **found** (with shard-file and byte counts across **every** snapshot that holds it, cumulatively — a session stays readable through older snapshots), **absent** (proved by a complete read of every snapshot), or **unknown** (the destination could not be read to completion — never spelled "absent"). Nothing is deleted: no archive, stage, destination or index is changed, and the preview never decrypts a session, only lists metadata.

| Flag | Meaning |
|---|---|
| `--destination <name>` | One destination to inspect, repeatable. |
| `--all-destinations` | Inspect every declared destination. Explicit: with neither this nor `--destination` the run is a usage error — there is no implicit destination set. |
| `--machine <id>` | The machine partition of the target identity. Default: this machine's partition. |
| `--confirm SESSION` | Refused (exit `3`): the destructive transaction does not exist in this build, so nothing was read or changed. A value that does not name the exact session is a usage error (exit `2`) either way. |
| `--dry-run` | Accepted and implied. Scripts can say what they expect. |
| `--json` | One object: per-destination status, occurrence and byte counts (unknowns as `{"kind":"unknown"}`, never `0`), snapshot ids. |
| `--repo`, `--key-file`, `--connections`, `--option` | Single-repository mode, as everywhere. |

The identity is exact: the session id as named, in one machine partition. The purge does not touch the source conversation, the staged copy, or any other machine's data — so a later `push` can archive the session again; "purged" would describe the selected destinations at the moment of the result, which is one of the reasons the destructive design requires a tombstone before any rewrite.

Exit codes: `0` every selected destination was read to completion (each `found` — a would-purge inventory — or `absent`) · `3` a destination could not be read to completion, so the target's status there is unknown and an absence proves nothing · `2` usage.

### `redact`

Refused (exit `2`), and nothing is read or changed: redaction — cut one session at a described boundary, physically remove the old occurrences, seal a user-edited replacement — is the destructive transaction of a later phase ([design](../docs-dev/redaction-and-recollection.md)). The command exists so asking for it gets the phase boundary rather than an unknown-flag error. Until then, `purge SESSION` previews a whole-session version and `hide` is the reversible one.

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
Grok CLI's `session_docs` row carries searchable plain text and a title, but not
message roles, turn boundaries, or per-message timestamps. The reader shows
that text under `Speaker unknown`; the activity index uses only the row's
session-level `updated_at` and title.
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

An index has no `--no-collapse` flag; like `read` and `export` it collapses legacy
whole-content replays while retaining verified sequence-bound captures, including
identical captured bodies. Setting
setting `CHAT_STASHER_NO_COLLAPSE` to any value other than `0`, `false`, `no`,
`off` or empty makes the next build index every stored shard instead. This
covers the full-text index here and the activity index (`activity-index`,
`overview`), which share the same reader. Clearing and rebuilding the index after
changing the variable is what makes the difference visible.

### `cache`

The body cache: the destination's own encrypted bytes, retained from
eligible single-session reads so the next read is fast. Nothing in it
is decrypted, and deleting it only costs speed.

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

## Importing an official export or a local-cache backup

### `import`

Two modes share this command.

With a `<platform>` and an `<export>` file, `import` turns an official export — a takeout **you** requested from the platform — into ordinary inbox bundles, so a conversation the browser extension never captured can still be archived. The export file is read whole and stored byte-exact under `<stage>/import-raw/<platform>/<sha256>` — a sibling of `sessions/`, so it enters no session count — and every conversation the platform's parser reads becomes one `chat-stasher/inbox@3` `web-capture` bundle in `--inbox`. A later `ingest --inbox` is what seals those bundles: this mode never seals, never pushes, never reads the config, a destination or a machine identity, and never modifies or moves the export file.

Claude's `conversations.json` and Grok's `prod-grok-backend.json` have parsers in this build; naming any other platform is a named refusal, not a guess. The archive comparison that would suppress a body already archived sits behind a probe this build ships unwired, so every conversation currently gets a body.

| Flag | Meaning |
|---|---|
| `--inbox <dir>` | Required. The local inbox folder the bundles are written into, for a later `ingest --inbox`. |
| `--stage <dir>` | Required in both modes. An existing stage directory that receives the byte-exact export under `import-raw/`, or the sealed shards of a backup import. Never created. |

The export file is only ever read: it is not moved, renamed or deleted, so importing from it again is safe.

The other mode, `--harness grok-bot --from <dir>`, imports one folder at a time out of local caches you copied aside — backups that may overlap the live cache, each other, and be shaped either as the persistence directory itself or as a whole app-support tree above it. For each session found it delivers only rows that session has never archived (the identity is machine + agent uuid, never a file path), so overlaps deliver exactly their new rows once and re-running the same import delivers 0 rows. The backups are only ever read and the live cache is never touched; an ordinary `collect` after an import still delivers only what is genuinely new, because the delivered state the two share is kept per destination — name the same `--destination` your collector uses. Each sealed shard's measurement line records that it came from a backup import, the backup directory's name and the import time; no absolute path enters the archive.

| Flag | Meaning |
|---|---|
| `--harness grok-bot` | Selects backup-restore mode. Only Grok Bot has a backup importer in this build; any other harness is a named refusal. |
| `--from <dir>` | Required in this mode, repeatable. The backup directory to read, read-only. |
| `--dry-run` | Report the per-session counts (rows new, rows already delivered) and write nothing — not even a state directory. |
| `--machine <id>` | The machine partition, as for `collect`. Default: config `machine`, else this machine's identity. |
| `--destination <name>` / `--repo` / `--key-file` | As for `collect` — the destination whose delivered state the import extends. |
| `--shard-bucket-cap <n>` | Maximum sealed shards per bucket (default: 20). |

Exit codes, both modes: `0` everything was read to the end and imported · `1` it was read, and a conversation, session or write failed — what was written is real but the run is incomplete · `2` usage (including a `--stage` that is not a directory) · `3` an input was never read to the end, so nothing about what it holds is proven and (for a backup import) nothing was imported.

## Recovering older history

### `backfill`

The Grok Bot local cache is a bounded window, and every position that fell out of it is gone from disk for good — but the server still holds the conversation, and whenever the app opens one, the app itself fetches the conversation's whole user-visible span into view. `backfill grok-bot` is the opt-in route through that fact: it quits and relaunches Grok Bot with a debugging connection that listens only on this machine, waits for the relaunched app to present its conversation list before reading it (a freshly relaunched app draws that list a while after it starts, and an empty list during boot is never taken as "no conversations"), opens one conversation at a time exactly as a person clicking the sidebar would, waits while the app loads it, and delivers the rows the archive does not already hold — through the same session identity a collect uses, so nothing is added twice and an ordinary collect afterwards stays incremental. Each delivered shard's measurement line records that it came from a backfill, with the run id and the app version it drove. The withheld interior rows the service never sends the app stay missing and the session's gap record and `partial` fidelity stand. macOS only; the tool never calls the vendor's service itself and never touches a credential. The app is restored to a normal launch at the end of every run, interrupted runs included: what has been delivered stays delivered, the debugging relaunch it quit is waited out — every recorded process fully exited and its debugging port released — before the plain relaunch starts, so the relaunch cannot lose to the dying instance's one-instance lock, and the report states the restore's own verification facts.

**The default is a dry run**: with no extra flags the same read-only traversal runs — it too relaunches and drives the app — and it only reports, per conversation: how many rows the archive would gain, the loaded span versus the locally held span, and the positions that will stay missing. Delivery needs the explicit acknowledgement flag, whose name is what the [source page](sources/grok-bot.md) documents.

| Flag | Meaning |
|---|---|
| `<harness>` | The harness whose app-driven backfill this runs. Only `grok-bot` exists today. |
| `--stage <dir>` | Required. An existing stage directory; never created. |
| `--i-accept-the-risk` | Deliver the rows found (a real run). Without it the run is a dry run: identical traversal, reports only. |
| `--conversation <prefix>` | Restrict the run to the conversations these id prefixes name, repeatable — the prefixes the dry run prints. Default: every conversation the app's sidebar lists. |
| `--machine <id>` / `--destination <name>` / `--repo` / `--key-file` / `--shard-bucket-cap <n>` | As for `collect` — the destination whose delivered state the run extends. |

Exit codes: `0` every selected conversation loaded and was reported (or delivered) · `1` the traversal finished with findings — a conversation that never finished loading within its wait and was skipped whole, a `--conversation` prefix that matched nothing, an app that could not be restored to a normal launch or a close-out that could not be verified (a relaunched app that did not stay running included) · `2` usage (including a `--stage` that is not a directory) · `3` the run refused before touching anything (macOS only, app not installed, a signed-in app unconfirmable, an unreachable or non-loopback debug endpoint), or stopped before finishing — a user interrupt, a relaunched app whose sidebar never listed a conversation within its readiness wait, or an app-layout change; partial work is kept and reported, and a close-out that failed on top of it says so in the same stop text rather than replacing it.

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
