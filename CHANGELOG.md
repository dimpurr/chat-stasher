# Changelog

Version numbers here are the CLI's, and they match the `vX.Y.Z` git tags. The
browser extension has its own version and ships on its own schedule; see
[`RELEASING.md`](RELEASING.md) for what a release contains.

## 0.5.0 — YYYY-MM-DD

Everything below has merged since `v0.5.0-rc.2`. The CLI moves to `0.5.0`. The
browser extension and the macOS menu bar app are separate artifacts with their
own version numbers, so what each of them gained or changed in this cycle is
under its own heading below.

### CLI

#### Added

- **Repair the duplicated-session read path.** Users who ran `setup` step 4,
  `dest-init`, or added a new destination on `v0.2.0` through `v0.5.0-rc.2`
  may see duplicated turns in those older builds. In `read`, `export`, FTS
  builds, activity indexes, and the repair inventory, a later run is dropped
  only when it is a byte-identical, shard-by-shard replay of that session's
  complete preceding shard sequence; other individual-shard repeats and
  matches against concatenated shard bytes are kept. The inventory reports
  those other individual repeats as suspicious and kept, and it **lists every
  collapsed run** — the session, where in its shard sequence the run starts, how
  many shards it spans and their bytes — so a collapse can be audited rather
  than trusted. New exact shard replays are no-ops at the writer. The collapse
  is a read decision and nothing is ever deleted: `read --no-collapse` and
  `export --no-collapse` return every stored shard, and setting
  `CHAT_STASHER_NO_COLLAPSE` (any value but `0`/`false`/`no`/`off`) makes the
  next full-text or activity index build index every stored shard too. Run
  `chat-stasher repair-duplicates --destination <name> --json` to inventory a
  destination. It is a dry run and never removes shards or snapshots.
- **Search inside conversation text.** `chat-stasher index build` builds a
  local full-text index for one named destination in the operating system's
  cache directory, `index check` re-reads and validates that index without
  contacting the archive, and `index clear` deletes it. The index matches
  literal substrings from three characters up, including in scripts whose words
  carry no spaces, so a four-character Han query works like an English
  one; one- and two-character queries are refused as too short for the index
  rather than answered with nothing to find. `search --text <query>` uses the
  index: the search reports whether the index covered every session the
  filters selected, and while any session was not covered, the run lists those
  rather than counting them as "no match", and the exit code says the answer
  is unproven rather than nothing matched. `search --text <query> --scan` reads the
  selected conversations instead and matches case-insensitively, which answers
  the one- and two-character queries the index cannot, at the cost of reading
  archived content.
- **A search page in the dashboard.** `/search` runs a query over the same
  index, as a plain GET form with no script, paged under the same contract as
  the session list. Each hit carries an excerpt that links into the reader at
  the message the index places the match in. A search that finds nothing says
  which of the states that is before it shows the zero: no index yet, with the
  command that builds one; an index that could not be read, with the repair;
  coverage short of the sessions in view; a read that did not finish; a query
  too short to evaluate; or filters no indexed document satisfied. What the
  JSON search reports for the same state is chosen by the same function, so
  the page and the machine reading the API cannot disagree.
- **One dashboard across several destinations.** `ui --destination a,b`, or
  `--destination all`, serves one merged dashboard: every destination named is
  read once, a session several of them hold is listed once with a badge saying
  how many copies exist, and both numbers stay visible, distinct sessions
  beside raw copies. Where two copies of a session disagree, the destination
  named first supplies the row, so the order you name them in is a choice and
  the page prints it back. The export command the sessions page prints for the
  sessions in view is refused under a merged view, because export always names
  one destination; every page a merged view can reach prints both session
  counts.
- **Getting a conversation out from the browser.** Each session page offers a
  download whose bytes are exactly what `read` returns for that session, and
  the export command the sessions page prints, ready to copy, quotes every
  value for the shell, so a path or filter value containing a space pastes as
  itself.
- **The session list grew handles for large archives.** The list is sorted and
  paged by the server, newest, by first or last message time, or by size, and
  every slice of it is a URL you can keep. Each row shows its label with the
  message count and the time state beside it, and sources group by what they
  are, web chats, coding agents, or an honest bucket for ids this build does
  not classify, with a facet bar over the groups.
- **Keyboard use of the dashboard, on every page.** Every page now opens with
  the same affordances: a skip link as the first tab stop that shows only when
  focused, a pages bar with one key each for overview, sessions and search,
  keys on the pager and on the reader's message windows for walking without a
  mouse, the footer naming every key where it works, and a focus ring that
  appears only for keyboard focus.
- **The reader renders the shapes the archive actually holds.** opencode,
  cursor and kimi-code sessions render as conversations. Each was routed
  before to a generic shape no capture path writes or to another harness's
  line format, so every such session rendered zero messages. Web bodies render
  on their active branch: a Claude conversation renders the chain of message
  parents up from the recorded leaf, a body whose leaf cannot be walked is
  shown in its own order and labelled as a branch nobody could pick rather
  than guessed, and a body with no parent links renders flat. A web platform
  whose conversation is a plain array renders that array's own messages
  instead of one unreadable bundle. A web session adopts the label its own
  body carries, ChatGPT's title or Claude's name, instead of none. A harness
  this build has no reader for gets the honest "raw view only" page, and
  nothing anywhere counts a record that was not rendered as if it had been.
- **Two steps of `setup` that were descriptive stubs are real.** The remote
  destination step asks which kind you want (SFTP, or an S3-compatible service
  such as R2), takes credentials by naming the environment variables that hold
  them, because no flag of this command ever takes a secret as its value,
  writes the one `[destinations.<name>]` block, and initializes that
  destination the way `dest-init` does. For a remote host nobody on this
  machine has met before it stops there: the first connection waits until you
  have compared the server's key out of band, exactly like the manual flow.
  The scheduler step installs the per-destination scheduler unit when you pass
  `--install-schedule`, and the completion summary reports the scheduler's own
  next run, or says plainly which jobs have no such answer to give: an
  interval unit and a not-yet-armed timer both do not, and a cadence is never
  printed as a timestamp. Non-TTY runs do the same work from named flags and
  print one JSON object, including anything that was missing.
- **Extension installs answer for themselves.** Each install of the browser
  extension reports through the host, at the end of each backfill tick, its
  browser, the profile label you gave it, the extension version, and per
  platform how much it captured and how much it still owes, with the pause
  reasons in words. The report carries counts and labels, never conversation
  titles or text. The host keeps those reports in the stage, so they ride
  ordinary archive pushes, and any machine's dashboard can therefore show
  every install's last report, not only this machine's.
- **An Extensions page in the dashboard.** `ui --view extensions` lists the
  archived reports above, one row per install, grouped by machine and sorted
  so the install that stopped reporting sits at the top rather than behind an
  alphabetical machine list. Each platform column carries the pair the counts
  are, labelled on the column header: captured by that install's own report,
  and still owed by it, never summed across installs or machines. A platform
  the report has no row for stays digitless and its label says so in words
  rather than reading as zero, a count the report omitted reads as unknown,
  and a paused platform names its reason. Only an
  install on this machine gets an open button, and it opens only through a
  browser profile this machine verified exists there and carries the
  extension; every other install says which machine to open it on. The open
  command is built by reading the browser's own local profile files to turn
  the label into a directory, and the privacy document names those files and
  states that nothing they contain is written back, sent to the host, or
  archived. A machine whose reports could not be read in full says so beside
  the list.
- **The native host registers every supported browser.** `install-native-host`
  now registers, per browser and per OS, the full set the support table
  names: Chrome, Chromium, Edge, Brave and Arc are supported and tested,
  Firefox carries its own manifest layout, and Chrome Beta, Chrome Canary,
  Opera and Vivaldi are registered best effort and marked unverified. On
  Windows a registry key is written only where one was actually located,
  never invented: a browser whose key is unknown is reported as written but
  not discoverable, rather than registered in a place nothing reads. A pair
  with no discovery path on the platform, Arc on Linux, which has no native
  build, is its own state in `doctor`, distinct from "you have not registered
  it", and the setup wizard's note about the extension
  now says to load it in every browser profile you chat in. The browser
  support table, one cell per browser and OS, is re-derived from the same
  registry the installer uses, so the README cannot promise a registration
  the tool refused to attempt.
- **Two JSON surfaces a menu bar or a script can poll.** `overview --json
  --summary` returns one aggregate, per-machine and per-source records and the
  last 30 local days, with no per-session array. `status --json` gains a
  `local` layer: whether the scheduler unit files are on disk, the most recent
  run, and how many staged sessions are still waiting to upload, and every
  count in it is an explicit known or unknown, never a silent zero.
  Destination credentials accept three more spellings, `file:PATH`,
  `env-file:PATH:NAME` and `keychain:ACCOUNT`, which resolve without a login
  shell, the situation a scheduled run and the menu bar app are in, and fail
  closed naming the missing credential rather than silently omitting the
  option; `env:NAME` keeps its documented lenient behaviour.
- **A read-only command that reports the packs no index file names.**
  `prune-orphans --destination <name>` surveys one destination without building
  an index and says how many packs it holds, which of them no index file names,
  how many bytes those are, and what the next push would do about them. Each
  unindexed pack is checked the way an adopting open checks one — its header
  decrypts, every blob it declares decrypts and hashes to the id the header
  gives, and the pack's bytes hash to the name it is stored under. The report
  carries the repository's own config id, the backend family and those counts,
  and no path, host, machine, account or conversation text, so it is safe to
  paste into a ticket; `--json` adds each candidate's full id. `--apply` is
  refused with exit 3, naming the three capabilities a safe delete would need,
  because "unindexed" means no index file names the pack, not that nothing uses
  it. A pack that could not be read is reported unknown and makes the whole
  survey incomplete, so "no candidates" is never claimed on a partial read.
  Nothing calls this command for you: it adds no schedule, no call from `push`
  or `run-once`, and no write path, and `append_only` is untouched.

#### Changed

- **Conversation counts count conversations.** A conversation archived on a
  laptop and a desktop under one id was counted twice, by `overview`'s
  headline, by the dashboard's stat tile and by the host's summary count, and
  those are one conversation observed twice. All three now say one, and where
  both numbers matter both stay visible: distinct conversations beside raw
  copies. `overview` also reports outright when one archive id carries records
  written by two different accounts, but only where the fingerprints one
  install itself recorded can be compared, because per-install salting makes
  another install's fingerprints incomparable, and it publishes the size of
  that blind spot rather than folding it into the collision count. The
  dashboard's collision banner counts conversations rather than rows for the
  same reason: every row of a colliding conversation carries the flag, so the
  canonical shape — one archive id on two machines — printed "2 conversation(s)"
  and listed the same short id twice, which is the per-machine double count the
  change exists to remove, restated in the sentence that names it.

#### Fixed

- **`collect` accepts long OpenClaw cold transcript filenames.** Hex encoding
  could expand a valid source filename beyond the 255-byte stage directory
  limit, causing `File name too long`. Oversized native and composed IDs now
  use a readable prefix and a deterministic 128-bit SHA-256 tag of the full
  value. IDs that already fit retain their exact bytes. Extension and raw inbox
  session directories apply the same bound; export reserves six bytes for its
  `.jsonl` suffix and keeps the canonical ID in the manifest.
- **`export --turns user` now reads the messages Claude Code records mid-turn,
  and stops reading notices as if a person had written them.** The filter kept a
  line only when it said `type: "user"` and was not a tool result. Two things
  were wrong with that. A message typed while the agent was working is stored as
  `type: "attachment"` with `attachment.type == "queued_command"`, not as
  `type: "user"`, so messages typed mid-turn were silently absent from the
  export. And `type: "user"` is also how Claude Code injects system reminders,
  task notifications and command echoes, and how it records compaction summaries and
  sub-agent prompts, so those were silently kept as if the person had written
  them. The rule now takes the `queued_command` records too, and where a record
  carries `origin.kind` that field is the whitelist — only `human` is the
  person — with the injected-prefix rule kept as the fallback for the older
  records that predate the field. A message recorded once mid-turn and once as
  a turn within fifteen minutes is written once; the text is never edited. The
  other tools still cannot answer the question from the archived line alone, so
  they are still refused with `turns_filter: "not-supported"` rather than
  silently filtered. For example, `chat-stasher export --destination <name>
  --out ./exported --harness claude-code --turns user` writes only the messages
  the format identifies as the person's own; other tools' lines are preserved.
- **Codex sessions are found on Linux and Windows.** The registry's cell for
  those two platforms spelled the store's location as the override variable
  itself — `$CODEX_HOME/sessions/`, `%CODEX_HOME%\sessions\` — which is not a
  form the scanner can expand, so on a default install `doctor` reported
  `codex not installed (…)` with `sessions=unknown`, `status` counted it among
  the harnesses "not probed at all", and `run-once` archived nothing from it.
  Exporting `CODEX_HOME` did not help, because the override is resolved by
  looking for the store's own directory layer in the template and that layer
  was not there any more. Both cells now name the documented default
  (`$HOME/.codex/sessions/`, `%USERPROFILE%\.codex\sessions\`) and keep
  `env_override = CODEX_HOME`, which is the shape every other harness in the
  registry already uses: an exported `CODEX_HOME` still wins, and the default is
  what the template alone anchors. macOS was never affected. `doctor`, `status`,
  `setup` and `run-once` all read the same cell, so all four are fixed together.
- **`doctor` says which clock its session dates are on.** The D3 line printed
  `earliest 2026-09-30T13:07:42Z` with nothing to say whether that was the
  session file's modification time or the conversation's own time — and the
  risk line below called it "your earliest session", so a session restored from
  a backup or copied to a new machine (fresh mtime, old conversation) was
  described as "about 0 days ago". Directory harnesses are probed from file
  metadata, so their rows now read `earliest(mtime)`; a single-file SQLite store
  carries the conversation's time in its own column, and its rows read
  `earliest(session time)`. The Gemini and Claude Code risk lines name the
  file-mtime clock too, and no longer state the session's *age* as the days
  remaining: a file written today said "about 0 days ago … only about 0 days
  left" under a 30-day window, and a session younger than that window could be
  reported as "already about -30 days past the 30-day threshold". Each now says
  how far the oldest batch is from the threshold, in whichever direction keeps
  the sentence true.
- **`doctor`'s coverage header no longer counts harnesses it never probed.**
  `N/12 known harnesses hit on this machine` used the whole registry as its
  denominator, so a machine where five cells were never opened reported a
  fraction that read as a measurement over all twelve. The header now reads
  `N/M probed harnesses hit on this machine · K not probed`, and the
  registry-driven table below it carries the same split. "Did not scan" is not
  "there is none", and the two numbers are now separate — the same distinction
  `status` already made in its own warning, from the same definition.
- **A destination's key file is now named wherever a destination is created, and
  the setup wizard asks for a backup of every key.** Each archive copy has its
  own key, and a second machine reads a destination with `masterkey-<destination>.json`
  and never with the local `masterkey.json` — so a user who followed the wizard
  literally, backing up the one file it named, could not read their off-site
  copy after losing a machine: measured on a real second machine as exit `3` and
  `cannot read masterkey file … (lost key?)`. The wizard now reports every key
  it asks about in `masterkey.keys[]`, each with its `scope`, `name`, `path` and
  `declared` state; `dest-init` names the destination key it created on stdout;
  and `doctor` (and `status --json`) report which key files this machine holds,
  whether each is present here, and whether the user has declared a backup of
  it. `status` adds a line per destination whose key is missing here or has no
  declared backup, and stays silent otherwise, so its default body is unchanged
  on a machine with nothing to report. One `--masterkey-saved-elsewhere`
  declaration covers every key *already on this machine* when the run starts,
  and is recorded per copy; a key the run itself creates — a destination's,
  which `dest-init` makes — is not covered, because nobody can have copied a
  file that did not exist when they answered. That run stops with `2`, names the
  new file in `masterkey.keys[]` with `declared: false`, and the same command run
  again records the declaration.
- **A key file that cannot be read no longer reads as backed up.** A declared
  backup is a statement about a file — that path, holding those bytes — and the
  comparison that decides whether a declaration covers a key folded every read
  error into the one error that means the file was deleted, which keeps the
  statement standing. So a key replaced by something unreadable — a directory
  at the key's path, a file with its read permission gone — was reported as
  declared saved, on the strength of a record about bytes nobody could read
  back. That is the one direction this file exists to prevent: an unchanged
  path proves nothing when the file cannot be opened. `doctor` and `status`
  now report the key as its own third state — unreadable, unknown whether it
  is still the declared file — and never as a plain "not declared" either,
  because a user whose declaration is on file must not be sent looking for a
  step they already did. `status --json`/`doctor --json` carry it as
  `declared_state: "unreadable"` next to the existing `declared_saved` field,
  which reads `false`; the setup wizard counts an unreadable key as not
  declared, so the step stays owed until the file can be read and compared.
  A missing key file is unchanged: the statement was made about the copy the
  user keeps, not about this machine's disk, so it still stands.

  The interactive declined prompt is pinned by a test that drives the real
  path — the binary on a terminal, an answer typed at the printed prompt,
  judged on the exit code and the record — which fails on the code the fix
  replaced, where a declined prompt was reported as a made declaration.
- **The `keychain:ACCOUNT` credential reference is macOS-only now.** On every
  other OS it is refused up front, with the reason that the macOS keychain
  does not exist there, instead of trying to run a tool that cannot be
  present and reporting a local setup that looks broken. Fail-closed is
  unchanged on any platform: the reference is refused, never silently
  emptied, and `file:`, `env-file:` and `env:` keep working.
- **A push that was killed no longer uploads the whole payload again.** A push
  writes its packs before its index, so one interrupted in between leaves
  complete packs that no index file names. Because each stored object's id
  covers a fresh random nonce, the same conversation never re-derives the same
  name, so a retry re-sent everything and the stranded bytes stayed behind for
  good — measured on a 560 MB payload at three kill points, and at test scale a
  retry added 1,047,572 B over a 1,045,700 B repository. Opening a repository
  now reads the header of every pack no index file names, so the dedup test
  finds those blobs already there and uploads none of them, and `push` reports
  what it found and what it did. A pack is adopted only after its own bytes
  check out: the header decrypts and parses, the lengths it declares match the
  file, every blob it declares decrypts and hashes to the id the header gives,
  and the pack hashes to the name it is stored under. A pack failing any of
  those is refused whole and named by id — the alternative was losing content,
  because a damaged pack either re-sent silently or entered the index so a
  later backup skipped bytes no reader can decrypt. Nothing is deleted, moved
  or rewritten.
- **A push of an unchanged stage adds no bytes.** A second push of a stage
  nothing had changed in could still upload tree bytes, so on Windows every
  scheduled no-op push grew the archive a little while reporting every staged
  file unmodified and no content blob written. The bytes came from stored
  fields no change in the files had touched: a file's `ctime` — on Windows,
  the creation time the platform reports — and a directory's own times, which
  the platform can report differently to two consecutive walks of one
  unchanged directory. A push stores neither any more; a value that never
  measured anything does not belong in the stored tree. A file's mtime is
  kept, because that is what change detection reads, so a changed stage still
  re-uploads and an unchanged one still reports every file unmodified.
- **A session file whose last line has no trailing newline no longer creates
  a new snapshot on every pass.** Only newline-terminated lines were ever
  sealed — an unterminated final record may be half of a write — but the
  collector re-read that tail on every pass and still counted the session as
  changed, so every `run-once` pushed a fresh snapshot of an unchanged stage,
  re-adding the same bytes each run and never converging: measured on the
  Windows machine as one more snapshot on each of four passes over one
  stable stage, and `setup` reported its idempotence link as not observed
  while still calling the run healthy. The rule is now decided and stated: an
  unterminated final line is *in progress* — it is re-read from the committed
  offset on every later pass and is committed in full by the pass that first
  sees its newline, so a tail is never lost and the sealed prefix is never
  re-staged. A pass that read the tail but sealed nothing counts the session
  as unchanged, so a source that stops changing converges to a no-op pass
  and zero new snapshots, and the read that observed the tail is still
  reported as bytes read. A `.jsonl.zst` rollout (a real class: codex
  compresses idle rollouts) held the same tail in progress through the same
  rule but churned through a separate hole: a pass that decoded it and found
  no complete line recorded a zero cursor — an offset of 0 and the digest of
  nothing, which can never match a nonempty source — and set its reset flag
  unconditionally, so every later pass re-decoded the same bytes and still
  counted the session as changed, and for compressed sources the convergence
  sentence above was false. Such a pass now records the source it observed,
  compressed length and digest — the same all-or-nothing cursor a sealing
  pass writes, since decoding has no partial positions to offer — so an
  unchanged source with nothing sealable answers from the remembered digest
  as a no-op pass with no reset and no snapshot, and the first pass after
  the newline arrives no longer matches, re-decodes, and seals the record in
  full. A file that never gains the newline keeps its tail
  out of the archive rather than sealing a possibly torn record; of 2,180
  real session files measured across macOS and Windows, none ended without
  the trailing newline.
- **A truncated pack is refused instead of crashing or hanging.** A metadata
  pack shorter than the index records made `read` panic (exit 101) and
  `verify` never return, on a repository with the default metadata cache; both
  are neither a diagnosis nor a measurement, so neither can stand in for "did
  not finish reading". A read now checks, before fetching a pack byte, whether
  any pack the index names is shorter than it records, and refuses with exit 3
  and the pack's id. A *missing* pack is deliberately not that finding: its read
  returns an error rather than a short buffer, which is what `verify` exists to
  report. Neither part is a timeout, because a wall-clock limit cannot tell a
  wedged reader from a slow remote and this project verifies archives over SFTP,
  where slow is normal. `verify --level all` also stops at the first level that
  could not finish reading and prints the remaining levels as NOT ATTEMPTED,
  counted in the summary, so "we did not check this" cannot read as "this
  checked out".
- **A stored organization is no longer read as an account.** An older build
  wrote an organization where an account belongs; two accounts can be members
  of one organization, so every value derived from that scope was equal for
  both, and a conversation captured under one was recorded as having come from
  the other. That is a positive false statement in an archive that cannot take
  it back. The activity index now requires the contract's person-bearing source
  and records no account key for anything else, and an install's coverage
  report answers from the plan rather than from the record, so a lease an older
  build wrote cannot be handed on as this scope's account. An envelope with no
  source, or one the contract's list does not name, is a bundle this reader
  cannot read rather than one it may assume holds an account.
- **A `setup` run that exits 2 writes what it says it writes.** A non-TTY run
  checked required parameters after the local first save, so a run that then
  refused had already created a repository and a masterkey — a side effect the
  missing-parameters contract forbids. The check now runs first, and an absent
  or incomplete named destination refuses before the archive pass. The
  exception that made the old behaviour look deliberate is kept and is the only
  one: when the masterkey declaration is the *only* parameter owed, the run
  creates the repository and its key and reports the key's path, because the
  declaration is an attestation about a file that has to exist before it can be
  made — and still exits 2, without running the archive pass, the remote step
  or the scheduler.
- **The installer's PATH advice names the directory the binary went to.**
  `install.sh` printed the same advice for every reader — add `~/.local/bin`
  to your `PATH` — even when `CHAT_STASHER_INSTALL_DIR` had put the binary
  somewhere else, and its closing line then said to run a bare
  `chat-stasher doctor`, the one command a reader who followed that advice
  could not run. The advice now names the directory the binary is in — the
  default is still spelled `$HOME/.local/bin`, so the line a reader pastes
  into their shell profile also survives a moved home directory — the closing
  line gives the binary's full path whenever that directory is not on `PATH`,
  and an install directory given with a trailing slash no longer reads as
  off-`PATH` or prints a doubled slash. The README's Quick start carries the
  same export line, so its instructions no longer stop at a command that
  would not be found.
- **A failed `schedule install` no longer leaves its unit files behind —
  or an "installed" claim.** With no user systemd session (WSL with
  `systemd=false`, measured there), `schedule install --format systemd`
  wrote both unit files and then failed at `daemon-reload`; the files
  stayed, no timer was armed, and `status` reported `installed: true` —
  the one failure mode where the command whose whole job is "is the
  scheduled archive working?" gave a false affirmative. A failed install
  now rolls itself back: it stops the timers it had got enabled, removes
  the unit files it created and restores the ones it replaced, so the
  machine ends a failed install the way it began it. And the systemd half
  of `status` asks the manager instead of trusting the disk: `installed`
  requires every unit file present *and* systemd confirming each timer
  active, and files without that confirmation are a new `unconfirmed`
  kind — with the reason beside them in `next_run_why` ("the manager did
  not report one", or it could not be asked at all), never a zero or an
  install claim. **macOS gets the same treatment.** The same false positive
  was measured on launchd: a failed `bootstrap` left the plist on disk, no
  agent was loaded, and `status` called it installed. So a failed launchd
  install rolls itself back too — the plists it created are removed, the
  ones it replaced get their previous content back, and the agents it had
  unloaded are loaded again — and the launchd half of `status` asks
  `launchctl` whether each agent is loaded before it claims an install.
  "launchd answered that it is not loaded" (a plist written by
  `schedule --output` and never bootstrapped is the ordinary case) and
  "`launchctl` could not be asked" stay two different sentences in the
  same `next_run_why`, and a plist launchd will not reach is no longer
  reported as a next run.
- **`schedule` on Windows refuses instead of writing foreign unit
  files.** There, `schedule install` wrote **systemd** unit files into the
  user profile — for a service manager Windows does not have — failed to
  load them, left them behind, and let `status` call them installed, while
  `schedule uninstall` failed the same way and left no supported way to
  remove the debris. Every `schedule` action on Windows now refuses with a
  message naming the manual Task Scheduler steps, exit 2, and nothing
  written; the message also names the leftover
  `chat-stasher-run-once-*.service`/`.timer` unit files an earlier build
  may have written and says they can be deleted. The manual steps are
  spelled out rather than gestured at: `docs/schedule.md` carries a
  copy-pasteable `schtasks /Create` command for an hourly per-user task,
  the `/Query` and `/Delete` that address it by name, what to read in the
  query output, and the caveat that a task created without `/RU`/`/RP`
  runs only while its user is logged on — which is also why this project
  does not ship such a wrapper itself. Task Scheduler
  integration through `schtasks.exe` was considered and rejected for this
  change, and the reasons are properties of that tool: a per-user task it
  can create without a password runs only while its user is logged on, so
  an archive that looks scheduled silently never runs on machines that
  reboot or log off; and the query output that would feed status and
  next-run is localized per Windows display language, so a parser this
  project cannot validate on real Windows hardware would report unknown on
  machines it was never checked on. The manual Task Scheduler setup was
  always the documented Windows configuration, so no capability is
  withdrawn — the pretence that an installed timer could be checked is.

#### Security

- **The activity index redacts the Windows profile directory too.** The
  redaction replaced `$HOME` with `~` only, and Windows normally leaves
  `$HOME` unset, so an error it emitted could print a path under your user
  folder. Both spellings the environment declares are now redacted, on every
  platform.

### Browser extension

#### Added

- **The extension without any CLI is a supported state, not a breakage.** The
  popup now shows a first-run card: what this is, why the half that writes
  files has to be a separate program on this machine, a one-line installer
  with a copy button, and an export button for captures not delivered. A
  persistent notice states the plain fact of that state: what it has exists
  only in this browser and is not a backup yet. "Never connected" and "was
  connected and stopped answering" are two different sentences, the first
  teaching the installer, the second pointing at the last stage the host was
  known to write and the repair command spelled with it. The popup probes on
  every open, and no failure to answer is read as proof the CLI is missing.
  Only a reply that succeeded at some point makes the popup offer a
  `chat-stasher` command. The gauge of the waiting area explains itself:
  at 80 percent the non-urgent backfill leg pauses, at 100 percent new
  captures are refused as before, and in both cases not one queued capture is
  dropped; an area that cannot be read is stated as unreadable rather than
  drawn as empty. When a CLI turns up later the backlog drains by itself and
  the popup says how many were delivered, on the evidence of a drain that
  actually emptied something rather than the mere fact that a host answered.
- **Every install has an identity and a name.** The extension generates an id
  on its first run in a profile and shows itself as "This browser: <browser> ·
  <profile>". Browser APIs expose no profile name, so the popup asks you to
  name the profile, and a profile that has not been named yet sends a name
  that says so, as a placeholder rather than an identity claim. Every capture
  bundle, coverage report, status report and export below carries the
  identity, so two installs of the same account can finally be told apart
  after the fact. When the identity store cannot be read, a capture is
  refused rather than sent with an invented identity, and the popup and
  console name the failure.
- **Export files name their install.** The undelivered-captures export file
  is named with the export time, the first characters of the install id and
  a per-export nonce, so two profiles exporting into the same downloads
  folder in the same second no longer write the same file. The bundles in it
  now carry the content fingerprint inside them: after `ingest` the host can
  still answer `has` for those conversations, and the same bytes delivered
  later are recognized rather than archived a second time. Exports written
  before this change import unchanged.
- **ChatGPT backfill walks the workspace the browser is signed into.**
  Backfill reads the workspace id out of the page's own outgoing requests,
  because that is the only place this build can observe which workspace the
  browser is actually using, and enumerates that workspace rather than
  whatever the account's conversation list happens to return. When the
  observation names two different workspaces it halts as ambiguous, and when
  it names none it halts as unresolved; neither is walked, and the coverage
  row names which of the two it was instead of showing progress against a
  workspace nobody identified. Reading back a workspace the account is not
  signed into is how a backfill files one person's conversations under
  another's, so failing closed here is the point.

#### Changed

- **A platform that answered "too many requests" slows this profile down for
  the rest of the day.** A 429 used to cost a wait of minutes, after which the
  leg resumed the very rhythm that had just been refused. Now the refusal is
  remembered in this browser profile: for the rest of the **local** day every
  request to that platform is made at half the pace of whichever speed preset
  is in force, and one tick fetches at most one conversation. The record holds a
  platform id and two timestamps and is forgotten by itself at the next local
  midnight, so nothing has to be cleared by hand and restarting the extension
  changes nothing about it. This brake is per install on this machine — it is
  not the machine-wide cooldown below, which the native host shares between
  installs.
- **Backfill is coordinated per machine.** Platform leases, request pacing
  and rate-limit cooldowns are now shared across every extension install on
  the machine, through the native host: when more than one install backfills
  the same platform the host runs one at a time and the rest stay paused for
  it, and a 429 that one install received stops that platform for all of
  them, honouring `Retry-After`; the shared lookups Claude backfill makes
  before it can ask for a list ride the same coordination. None of this
  crosses machines, and there is no server between any of them. A CLI old
  enough not to arbitrate cannot run one of these shared windows, so
  backfill does not run and the coverage page says to update chat-stasher to
  enable it, while live capture never pauses.
- **The leases are per account, and the account itself never travels.** A
  fingerprint one install records is deliberately incomparable to another
  install's, so coordination compares a key derived locally from the
  machine's masterkey instead. The same account signed into several installs
  coordinates as one account, two different accounts each hold their own
  window for a platform, and only the derived key ever reaches the host.
- **A backfill no longer files another account's conversations.** A run
  records the account it starts under and checks every list page and body
  against it, halting before anything is enqueued or settled if the answer
  names a different account. A live capture that shows a different account
  suspends every other scope of that platform whose recorded account
  disagrees, and a suspended scope keeps everything it owes until its account
  is seen again. An account the traffic does not name is never treated as a
  switch: a scope with nothing comparable recorded stays untouched instead
  of accused. The coverage page names which account a scope ran for, or that
  none was ever recorded, and the suspended state promises neither a countdown
  nor permanence.
- **ChatGPT conversations keep their project, with room to say it is
  unknown.** A capture that cannot tell which project a conversation belongs
  to records that explicitly as unknown instead of recording it as nothing,
  and a later observation can supply the effective project as a separate
  attribution, with its source and when it was seen, without rewriting the
  capture. `search --json`, `overview --json` and the dashboard's session API
  carry those states, and "nothing was recorded" is never printed as
  "recorded as unknown". A page cannot author these claims: project names and
  workspace ids are metadata the backfill leg records, in plaintext inside
  this machine's archive, as the privacy document states.
- **The popup's words match what it can know.** The extension's own tallies
  are worded as captured by this browser, because that is all this install can
  see; what a push has saved is the archive's fact and no longer borrows the
  extension's words for it. The note about a platform's own list disagreeing
  with what this install still owed says whose records were compared: this
  browser's. The two counts are never mixed and never added together.

#### Fixed

- **A hook failure a page stopped reporting is retracted.** A top document that
  half-installed, wrote that its hook had been replaced and then closed left the
  popup asserting a page-state that had stopped being asserted, and nothing
  could withdraw it: the remedy the popup offered reloads the embedder, not a
  top document on that origin. A page still in the state it reported re-reports
  it on a fixed interval, so a record whose observation has not been re-reported
  for five minutes is pruned before the popup builds its model. The window is
  absolute rather than a multiple of the reporting cadence, because the browser
  throttles background-tab timers to about once a minute and a genuinely broken
  but merely backgrounded page must never age out. The prune is scoped to the
  release channel: a dev and a stable build share one extension id and one
  storage area, so a stable popup can no longer delete the record a dev build
  is the only witness of.
- **A conversation list that never changes no longer ends an enumeration
  silently.** The guard that asks whether the parameter the engine advances
  actually moved ran for token paging and for one offset plan, but ChatGPT is
  offset-paged and the engine advances that offset itself. A server that ignores
  the parameter therefore returned the same page for ever — one page per tick,
  the same page back, nothing added, and every stored field still reading as
  healthy. Every offset-paged plan now asks, and an enumeration that stops
  making progress halts rather than reporting health.
- **An organization is no longer recorded as an account**, the extension half of
  the CLI fix above. claude.ai addresses every conversation by organization and
  two accounts can share one, so the value a capture recorded as its account was
  equal for both — a positive, false statement about who a conversation came
  from. Such a capture now records `organization-is-not-an-account` and creates
  no salt, the run lease answers `scope-names-an-organization` rather than the
  false "this plan declares no account axis", and the coverage page tells the
  three facts apart from the plan rather than from the record, so a lease an
  older build wrote cannot be read back as "the account this scope belongs to".
  The organization is not lost: it is in the bundle's own URL and it is still
  what the host coordinates by. It is the recording of it as an account that
  stops.
- **The account-switch sentence no longer prints a dangling quote.** The one
  sentence this feature ships — the popup's headline for a leg that stopped
  because a different account had been signed in — ended in a stray `"` inside
  a folded scalar in both language catalogues, so what the reader saw was the
  headline about a stopped capture with a quote mark hanging off the end. No
  gate reads locale prose, which is why this had to be caught by reading it.

#### Security

- **A copied profile is detected from its own future.** A profile copied, or
  restored from a backup, carries the install identity along with the rest of
  the browser's storage, so the copy and the original report under one id, and
  nothing in that storage can tell them apart. Each install keeps a counter that
  rises with every report and every delivery, and two live writers on one id
  eventually send a value the host has already recorded. That observation is
  sound only if one writer's values arrive in order, so a number alone is not
  the evidence: each allocation mints a random nonce beside its number and the
  two are sent together. One number under two different nonces is two writers
  reserving it independently, and marks the id until a new one is minted; the
  same pair twice is one allocation arriving twice and is answered as the
  duplicate it is. Order is evidence of nothing — a late value, a higher one and
  a retry are all accepted — because native messaging hands each message its own
  host process and a send that times out is retried. On detection, deliveries
  are answered with a retryable item-scope refusal so captures stay queued rather
  than being filed under an identity that cannot be attributed to one profile,
  and backfill claims stop. A number older than the host's recorded window is
  accepted without judgement, and the contract says so rather than reporting a
  comparison it did not make. Nothing rotates an identity on its own, because a
  restored backup or a renamed profile would then have the tool sever the wrong
  lineage. The staged status record's conflict flag is read and published inside
  one transaction, so a report delayed between its own observation and its file
  cannot erase a conflict a faster copy already published — a dashboard showing
  a clean install whose captures the host is refusing is the one direction this
  must not fail in.
- **Copied profiles are refused, and told how to fix themselves.** Every
  sealed record on the machine now carries the producing install's browser
  and profile label. A profile copy that delivers the same install id under a
  different browser, or under a profile name that differs from the one its
  user actually gave it, is refused with a non-retryable instruction to
  regenerate its identity, and while it keeps that id its captures stay
  queued rather than merging into the original's records. Naming a profile
  after captures made under no name remains the normal first-run flow and is
  never read as a conflict. What this catches is deliberately bounded, and the
  bound is honest: two copies that keep the identical browser and identical
  profile name are not distinguishable without a server, and the comparison
  only works inside this machine's stage.

### Menu bar (macOS)

A menu bar app joins the CLI and the extension. It ships as its own notarized
disk image with its own release steps, outside the CLI's asset set, carries no
version of its own (following the CLI's), and reads only the two JSON
surfaces described under CLI above, never the archive itself. It needs the CLI
on `PATH`, and macOS 13 or newer.

#### Added

- **Archive status in the menu bar.** The app shows overall archive health,
  one conversation count, and it counts conversations, never a sum of
  destination copies or machine profiles, per-machine freshness, the
  per-source list
  behind a Show sources row, and a 30-day activity strip. A source counts as
  in use after activity on three distinct days, and the threshold that calls
  it stopped comes from its own observed cadence. Open dashboard starts
  `chat-stasher ui`. A failed refresh keeps the last good result visible with
  an Offline label, and an unknown count stays unknown on screen, never a
  zero. A machine with no activity index is its own condition on both
  surfaces: the popover says how many machines are missing one and that the
  coverage is incomplete, and the bar's health line counts those machines as
  needing attention rather than as silent.
- **Destination choice in Settings.** With several destinations configured,
  the app checks each one and shows the worst state by default; Settings can
  pin it to one named destination instead, a launch-time environment
  override still wins, and the destinations' conversation counts are never
  added together.
- **It names the CLI it found.** The app resolves the binary the way a
  `PATH` search does and runs that same file every time, and the About sheet
  names the absolute path and the version. Its states are distinct: no CLI
  found; a CLI too old to answer, which is an upgrade instruction; a current
  CLI whose config cannot be used, which is the credential or setup card,
  not an upgrade that would change nothing; and a failing read, which keeps
  the last good result. A headless one-line handshake exists for scripting
  the same check, and Settings also holds launch-at-login and silence
  thresholds.
- **Signing and notarization from one script.** The script builds in release,
  signs every executable it finds with the Hardened Runtime and a secure
  timestamp, verifies the result, submits the disk image to Apple's notary
  service, and staples and validates the ticket. It refuses rather than
  improvising when the Developer ID certificate or the notary credentials are
  absent, and a self-check command answers ready, missing, or could not
  tell, which is its own answer and not a pass.

### Repository and release tooling

Nothing here is in the shipped binary.

- The README was rewritten for the person installing the tool, and the first
  reader-facing pages were added under docs/ covering the first archive, each
  platform, destinations, and privacy and security. Seven more followed —
  setup, schedule, cli, config, troubleshooting, how-it-works and support — so a
  reader can look up a first run, a flag, an exit code, a config key or a
  failure without reading the source, and the dashboard, the extension and the
  timer are described in one place rather than only in the README. Every
  command, flag, JSON field, exit code, path and status word in them was
  checked against a build of this tree. The engineering documents moved under
  docs-dev/, and a check now resolves every link between the project's own
  Markdown files, including the fragment on each link, so a moved file or a
  renamed heading fails a gate instead of rendering silently.
- A check that resolves the project's own Markdown was joined by one that reads
  tracked files for paths into a private working directory, because the policy
  forbidding them was adopted after some of them were written and nothing
  enforced it — nineteen files named one, and the worst published an absolute
  path into a scratch tree. The references are kept and so is their meaning;
  only the directory is dropped, so a report is named by its own filename and a
  competitor's source by the file being quoted rather than the directory this
  project cloned it into.
- The browser support matrix is generated from the same registry the native
  host installer reads, and one cell had published a link to a mirror of
  proprietary source. No vendor document for that row existed to link to
  instead, so the cell records its provenance in words: no vendor doc, the key
  carried from a third-party implementation, the source in the installer, the
  date it was read, and that it is not linked. The tier it sits in is unchanged
  on purpose. A cell with nothing recorded holds one plain hyphen rather than
  the typographic dash the tables carried, so the value survives any font or
  pipeline a copied table lands in and can be searched as text.
- Two registry steps the `0.5.0-rc.2` run tripped over are fixed. The crates
  registry's documented data-access refusal (HTTP 403) is no longer read as
  "that version is not published", which was the misread that could publish
  over an existing version, and npm's `latest` dist-tag is now compared
  numerically, so `0.9.0` cannot be restored over `0.10.0`. `latest` may be
  moved onto a release candidate only before the package's first stable
  release.
- The extension's end-to-end suite grows with what it protects. A Perplexity
  capture spec drives a real browser against a fake Perplexity that answers
  where the site's origin usually does, requires one bundle to reach the
  waiting area, and counts every request that tried to leave the machine,
  asserting none did. The privacy document's backfill summary now counts
  Perplexity among the platforms whose conversation text it can recover, with
  the long-conversation caveat the others carry. A second suite runs two
  extension installs, in two persistent browser profiles, against one real
  native host binary, so the multi-install story is tested by the same
  program the delivery path uses.
- A guide for coding agents working in this repository was added at its root,
  and its install path is the setup wizard, so an agent's first archive and
  its checks are the ones a person would run.

## 0.5.0-rc.2 — 2026-09-25

A release candidate for `0.5.0`, cut so that these exact artifacts can be
installed and run before the version becomes stable. It is not what an
unqualified install gets: `scripts/install.sh` pins the newest stable release,
so reaching this one means naming it (`CHAT_STASHER_VERSION=0.5.0-rc.2`).

`0.5.0-rc.1` was tagged but never released: the Linux x86_64 static check
rejected the binary the build had just produced — a static one, which `file`
calls `static-pie linked` rather than `statically linked` — and the run ended
before anything was published.

Compared against `v0.4.0`, tagged 2026-09-24.

### CLI

#### Added

- **Linux and Windows binaries, not only macOS ones.** A Release now carries
  `chat-stasher-linux-x86_64` and `chat-stasher-linux-arm64` — static musl, so
  one binary covers every distribution — and `chat-stasher-windows-x86_64.exe`
  beside the two macOS binaries. `install.sh` installs the Linux artifacts: it
  reads the Release's `SHA256SUMS` **before** downloading anything, and starts
  the downloaded binary once before moving it into place, so a binary that
  matches its checksum but cannot start is refused and whatever was installed
  there before is left alone. Windows is not installed by that script — it is a
  POSIX `sh` script — so that branch prints the `.exe` asset to download instead.
  The version that command installs without being asked stays the newest stable
  release, so a Linux install of *this* release is the one that names it.
- **`npm install chat-stasher`.** An npm launcher package resolves a
  per-platform package carrying the released binary; the platform packages are
  assembled from a Release's own assets after their checksums are verified.
- **`cargo install chat-stasher`** installs the CLI from crates.io, and
  `cargo binstall chat-stasher` finds a prebuilt binary for every released
  target.
- **`chat-stasher setup`** walks through the first run. It scans local sources,
  then performs the local first save: two `run-once` passes (the first creates
  the encrypted local repository and its masterkey, the second proves that a pass
  with nothing new to archive adds nothing), a read of one session back out of
  the repository through the same store and query `search` uses, and the
  masterkey path with the instruction to copy it elsewhere. Copying it is
  confirmed by typing a sentence, which the walkthrough records as a declaration
  it cannot verify: nothing checks that a copy exists. A non-TTY run does the
  same work from named flags and prints one JSON object, including any missing
  named parameters. The destination and scheduler steps are still descriptive
  stubs.
- **`chat-stasher cache`** reports this machine's conversation-body cache: where
  it lives, the quota it must stay under (`[cache] max_bytes`, 2 GiB by default)
  and how much of it is in use; `cache clear` deletes every cached block. The
  cache holds the destination's own **ciphertext**, is disposable — losing it
  costs only speed — and is never consulted by `verify`, `export`, `dest-init`,
  `push` or `read --all-machines`, which read the destination itself because
  proving the destination intact is the point.
- **A read-only `has` request in the Native Messaging protocol.** The extension
  names a bundle's platform, session id and content fingerprint, and the host
  answers whether the stage already holds that exact content, looking only in the
  directory a `deliver` of that bundle would write to and taking no lock.
  `held: true`, `held: false` and a `nack` stay three separate outcomes: the
  middle one is an answer, the last one is a question that could not be asked,
  and "already archived" is never concluded from the extension's own memory.
- **A label per session in the activity index.** Each session's row now records
  what a list should call it: the harness's own title when it wrote one, else the
  head of the session's first user line, capped at 100 characters and flagged
  when the cap cut anything. `search --json` and the dashboard resolve the label
  from that row, and the honesty rules are the ones the tool already applies to
  time: content read with nothing label-able in it records an explicit "no label
  recorded", and an index written before labels existed reads as "label unknown"
  at machine level. Neither is an empty string, and neither is a guess.
- **`ui` picks the destination itself when the choice is unambiguous.** With
  exactly one `[destinations.<name>]` declared it opens that one; with several it
  opens the one the native host names, or lists the declared names and exits `2`
  when no default is recorded. `search`, `export` and `overview` still require
  the copy to be named, always.
- **`schedule install` and `schedule uninstall`** write and remove macOS launchd
  agents idempotently, one unit per configured destination (`--destination`).
  `schedule` without a subcommand still only renders.
- **An S3-compatible destination is documented end to end**, and a backend
  option value may be spelled `env:NAME`: it is read from the process
  environment at run time, so an access key and a secret need not be written into
  the config file.
- **A "no conversation content" state**, kept apart from "time unknown"
  everywhere. A session archived with no conversation content — an empty shard,
  or metadata lines such as a summary with no user or assistant message — is
  counted and listed separately, is never placed in a time bucket, and does not
  make an answer incomplete, because there is no conversation whose time could be
  missing. `overview` gains `summary.no_conversation_content_sessions` and its
  own section; `search` and `export` gain a distinct unplaced reason for it; the
  dashboard and the view JSON separate it too.
- **A partly-read range is no longer reported as a complete span.** A session
  whose records were read but not all of them placed in time keeps the bounds it
  did measure and records them as inner bounds: a window those bounds do not
  reach is answered "could not be placed" rather than "not selected", because the
  unplaced record may be stamped in that very window. A window the bounds do
  overlap is still a real match.
- **kimi-code conversation time**, read from the records the archive holds; and
  the harnesses whose conversation time was wrong — grok, gemini-cli, perplexity
  — now report the bounds they actually measured.

#### Changed

- `ui` now starts its server even when the archive read matched nothing: the
  destination is served as an honest empty page (`(this destination holds no
  sessions)` on a complete read; a floor sentence when parts were unreadable)
  instead of exiting `1` before any socket was bound. `ui` no longer exits
  `1`; a served read that could not complete still exits `3`.
- `chat-stasher ui` names machines that hold sessions but have no activity
  index on the HTML overview: a banner carrying exactly the machine list of
  the `machines_without_activity_index` API field, and their heatmap rows read
  as unknown — `?` cells with the reason and the `chat-stasher activity-index`
  repair command on the page — instead of a row of empty cells.
- **A config file that exists but cannot be used is no longer replaced by the
  built-in defaults.** A config that does not parse, a value of the wrong type,
  or a path that cannot be resolved stops every command that reads it with exit
  `3`, printing the file, the position and the reason. Continuing on the defaults
  would run a scheduled `push` exactly as if no destination had ever been
  declared. `doctor` is the one command that keeps going, and it lists the checks
  it therefore could not perform; an **absent** config file stays what it always
  was — the normal first run, which does use the defaults.
- **An unreadable `[cache]` section no longer takes the whole config with it**,
  and a section whose values cannot be read leaves the body cache **off**: a
  mistyped quota must not activate a cache nobody asked for.

#### Fixed

- `ui`'s `[ui] sessions : N in view / M in the archive` narration quoted the
  launch-filtered count as if it were the archive count; it now quotes the
  archive.
- **`verify` distinguishes failing to read from finishing a read.** An observed
  integrity mismatch still exits `1`; a verification that could not finish
  reading exits `3`, so its silence about everything else proves nothing.
- **The reader no longer renders a message without a bound or invents what it
  was not given.** A message renders at most 64 KiB, with the dropped byte count
  printed where the text stops and the raw shards one link away; a timestamp the
  reader had to interpret is labelled as interpreted rather than shown beside a
  recorded one as though the two were the same claim; a mixed turn keeps its
  recorded role; and the empty and window states say what they are.
- **codex and gemini-cli bodies render in the shape the archive actually holds**,
  and a message node that carried no content is no longer counted as one the
  reader failed to render.
- **`install.sh` is a POSIX script now.** It is documented as `curl -fsSL … | sh`,
  and on Debian and Ubuntu `sh` is dash, which rejects the bash-only constructs
  the script used — so the installer was broken for exactly the users its Linux
  artifacts exist for.
- **The activity index is repaired by a pass that pushes nothing**, instead of
  staying stale until something else changed.
- **A Windows config path is repaired as a path**, control escapes and all,
  rather than as a string.
- **The string scanner knows a character literal is not a string**, so an
  apostrophe in a comment no longer derails what it reads.
- **Claude Code's own metadata timestamps are ignored** when conversation time
  is derived.

#### Security

- **The reader refuses a link target it cannot call local.** The predicate tested
  the first byte, so a target beginning `//` — a protocol-relative URL, which a
  browser resolves against the page's own scheme — and the `/\` and `\\` forms
  it maps to were emitted as links. A fragment is local, and a path is local only
  when the byte after its leading `/` is neither `/` nor `\`; anything else is
  printed as text, which is what the page's own footer claims happens.
- **The body cache refuses to write or delete outside a directory it created.**
  Clearing the cache, or evicting to stay under its quota, cannot be aimed
  elsewhere by a path in the config; a component that cannot be inspected refuses
  the operation rather than being read as absent.
- **Backend credentials can come from the environment** (`env:NAME`), so they
  need not be stored in the config file, and the documented S3 configuration sets
  both switches that stop the backend from looking for credentials anywhere the
  user did not name — the ambient AWS environment and profiles, and the instance
  metadata service. A missing or mistyped credential cannot then quietly
  authenticate as the machine's instance role.

### Browser extension

The extension has its own version, and this candidate does not move it: the zip
attached to this Release is built on the stable channel and named
`chat-stasher-extension-0.2.0.zip`, the same version the previous Release
carried. Everything below is in that zip.

#### Added

- **A coverage page** — a standalone extension page, plus a summary card in the
  popup — that shows what the extension knows about its own backfill, per
  platform and account scope: how much is owed, what has been settled, what was
  skipped and why. Its wording is held to the state the model actually reported,
  a total says where it came from, a truncated list's count is marked as a lower
  bound, and a scope that is unregistered is named as such even while it still
  owes work.
- **Speed presets** — gentle (the default), standard and faster — wired to the
  backfill leg's caps, and a doubled rate for platforms on the stable channel.
- **Target-registry cap evictions are recorded durably**, shown in the popup and
  disclosed in the privacy document, instead of being a silent eviction.
- **`Retry-After` is honoured on 429/503 responses**, including a response whose
  body cannot be read, and list-only pages are paced accordingly. A value that is
  not an HTTP date is rejected before it can be parsed into something else.

#### Changed

- **Backfill no longer trusts its own record of what it delivered.** A capture is
  treated as already archived only when the host answers `has` with `held: true`
  for that capture's own fingerprint; a remembered "we stored this" survives the
  archive being replaced or restored at the same path, and the conversation would
  then be settled as archived without a byte reaching the new archive. A
  delivery is also keyed by the destination that acknowledged it, so two
  destinations do not settle each other's work.
- **A repeated later page halts the leg**, not only a repeated first page, and a
  re-enumeration that legitimately repeats is no longer read as a changed
  response shape.
- **Each conversation's own list time is recorded**, and it is kept when its debt
  is settled, so a long backfill does not restamp conversations with the time it
  finished.

#### Fixed

- The retry bucket is worded from the reasons it actually holds, and the popup
  card and chart geometry are grounded on real surfaces — month labels stay
  inside the chart, and the duplicate-skip sentence the view could never reach is
  gone.

### Repository and release tooling

Nothing here is in the shipped binary.

- The release workflow reconciles a re-run's assets instead of growing the set:
  every asset the run does not stage is deleted first, the staged files are
  uploaded over them, and the run fails unless the Release's asset set then
  equals the staged set exactly.
- It assembles the npm packages from the Release's own assets and publishes them
  (`next` for an rc, `latest` for a stable tag), then publishes the crate to
  crates.io, skipping an exact version that is already published — and it refuses
  a run whose ref is a branch rather than a tag, or whose tag disagrees with
  `Cargo.toml`.
- The Homebrew tap update is automated behind a token: it opens a draft pull
  request against the tap and never merges, so the tap cannot make a release
  fail and a release cannot reach the tap unreviewed.
- The committed support tables (`scripts/support-matrix/short-table.md` and
  `full-table.md`) are generated from the harness registry and the extension's
  platform table, and CI fails when they go stale against either input.
- A Linux end-to-end smoke test drives `doctor` → `init` → `run-once` → `read` →
  `overview` → `schedule` against synthetic histories, and the installer's own
  self-test now runs in CI, including its `dash` cases.
- Several documents stopped promising Linux binaries that no Release carried, and
  the Homebrew surface's claims about Linux were corrected.

## 0.4.0 — 2026-09-24

### CLI

#### Added

- `activity-index --rebuild --destination … --machine …` rebuilds that machine's
  activity index from every archived snapshot. It is safe to rerun but starts
  over rather than resuming, and restarts if the same machine pushes during the
  rebuild. From issue #2.
- `search --json` and export manifests report per-machine recall with
  `located`, `time_unknown` and `index_trusted`; daily manifest entries also
  report `unknown_anywhere`. A `WARN` goes to stderr when at least half of a
  machine's candidate sessions have unknown time. From issue #2.
- `overview` and `status --destination …` show each machine's last writer
  version and flag versions behind the newest writer.

#### Changed

- Conversation time is now derived from message timestamps for ChatGPT, Claude,
  Gemini, DeepSeek, Grok, Perplexity and Kimi web captures. Export manifests
  mark `time_source` as `messages` or `list-updated`, and numeric epochs are
  interpreted as absolute timestamps. From issue #3.

#### Fixed

- `install-native-host --stage …` appends with the file's dominant line ending
  and refuses a dotted or implicit `stage` table.

### Browser extension (first stable release, 0.2.0)

#### Added

- Stable releases now ship the extension for ChatGPT, Claude, DeepSeek, Gemini
  and Grok; Perplexity and Kimi are available in development builds.
- Perplexity conversation bodies can be archived in development builds when the
  response proves the body is complete; incomplete or unproven bodies are
  refused.

#### Changed

- Claude history backfill now archives verified active branches, re-lists once
  to recover conversations an earlier walk dropped, and works from a newly
  opened `claude.ai/new` page by resolving its organization from that page.
- Gemini history listing now continues beyond the former request-body size cap.
- Backfill rotates fairly across platforms and accounts by least-recent service.
  Turning it off is honored during an active run, and the popup switch sends
  alarm changes through the worker's single synchronization queue.

#### Fixed

- An empty conversation no longer stops platform backfill: its id stays owed
  until a real body is archived, while three consecutive empty bodies halt the
  run as a signal that the endpoint may have changed.

## 0.3.0 — 2026-09-24

Compared against `v0.2.0`, tagged 2026-09-12.

### Added

- **`export --out <dir>`** writes the sessions a query selects to files:
  `<out>/<machine>/<harness>/<session-id>.jsonl` holds each session's archived
  lines in their native format, byte-identical to what `read` returns for it,
  and `<out>/manifest.json` records per session its machine, harness, id, first
  and last message time, shard count, bytes written, sha256 of the file on disk
  and the filters that were applied — plus the sessions no filter could place,
  the machines whose activity index could not be read, the sessions that could
  not be written, and the run's exit status. Selection is `search`'s own code
  path, so the written set equals what `search` reports for the same flags.
  `--dry-run` prints the cost and writes nothing. `--turns user` keeps only the
  user's own lines and only for harnesses whose format makes that certain;
  elsewhere every line is written and the session records
  `turns_filter: "not-supported"`. `--trim-to-window` also drops individual
  lines whose own timestamp falls outside the window; a line whose time cannot
  be read is kept and counted in the manifest's `untimed_lines`, and the flag is
  refused outright if no window was given. Exit codes match `search`'s. From
  issue #1.
- **The native messaging host now works.** At `v0.2.0`, `native-host` without
  `--self-test` printed "the stdio message loop is not implemented in this
  build" and exited 2. It now reads one length-prefixed request frame and writes
  one response frame, answering `hello`, `deliver`, and two read-only requests:
  `summary` (session counts in the stage, in the last 24 hours, and by harness,
  plus the last successful push — counts and timestamps, never text, titles or
  session ids) and `open_dashboard` (starts `chat-stasher ui` and returns its
  per-launch URL, including the access token, to the calling extension only;
  never logged, never written to disk). A `summary` or `open_dashboard` request
  carrying any field beyond `protocol` and `type` is refused with `bad-request`;
  `hello` and `deliver` ignore fields the protocol does not define, because
  refusing one would reject a real conversation for a reason no document states.
- **`install-native-host --stage <dir>`** records the stage a browser-spawned
  host is allowed to write to, as `[native_host] stage` in the config. The
  directory must already exist and be a directory — the host never creates a
  stage — the edit preserves the config file's comments and everything else in
  it, and the flag cannot be combined with `--uninstall`.
- **`doctor` reports whether the Native Messaging host is usable**, as a new
  `D8` section: the browser-side registration on this machine and the stage
  `[native_host]` points at. Read-only, like the rest of `doctor`.
- **`search` filters on conversation time.** `--day <local date>`, or
  `--since`/`--until` for a range, matches a session when its own activity
  interval intersects the window. A session whose time is unknown is listed
  separately with the reason rather than dropped, and while a window is active
  the exit code is `3` rather than `1`, because "0 matched" is then unproven.
  `--harness <id>[,<id>…]` matches the leading `.`/`~` segment of an archived
  session id — the same shared filter `export` takes — and a session whose id
  carries no harness prefix cannot answer that question, so it is listed as
  unplaced rather than counted as a non-match. `--json` prints one object on
  stdout in place of the human report, keeping matched, not-matched and
  could-not-be-placed as separate fields so a consumer cannot read an unknown as
  an absence. `search` is also the dry run of `export`, and both build their
  filter from one shared set of flags.
- **Kimi Code** is a known harness. Kimi Code names a session by its directory
  and always calls the transcript `wire.jsonl`, so the registry cell declares
  that shape: the native id comes from the session directory, and only
  `agents/main/wire.jsonl` under it is read. Everything beside the transcript —
  `state.json`, `logs/`, the home-level index — stays out.

### Changed

- **BREAKING — `search --since-unix` / `--until-unix` no longer bound the same
  thing.** The two flags still exist and still take unix seconds, but at `v0.2.0`
  they bounded a session's *rustic snapshot time*; they now bound that session's
  own conversation interval, and they are deprecated in favour of `--day`,
  `--since` and `--until` (local calendar days), which mean the same thing. A
  saved command that passed a snapshot time therefore selects a different set of
  sessions, and it can both gain and lose sessions: the old test asked whether
  the snapshot time fell inside the window, the new one whether the
  conversation's own interval overlaps it, so a conversation written before a
  later snapshot can now match a window the snapshot missed, and the reverse. A
  session whose time cannot be read is no longer compared at all but listed as
  unplaced, which makes the exit code `3` rather than `0`/`1`.
  **What to do:** re-run the command, read the hits, and move the bounds to
  `--day`/`--since`/`--until`. The flags print a deprecation notice on stderr and
  are removed in the next release.
- **`ui` replaces `view`.** `ui` opens the archive dashboard (totals, a
  per-machine health row, a machine × source matrix, a weekly activity
  heatmap) on `127.0.0.1`; a click through to a session fetches and decrypts its
  text, and prints the byte cost first. `view` remains as a deprecated alias for
  one release: it prints a one-line notice on stderr and behaves identically.
- The Windows discovery root is derived from the home directory instead of being
  read from the process environment. `nativehost::default_root` is now pure —
  the same `(platform, home)` gives the same answer on every machine, so a test
  with a temporary home, or a `--target-root` probe, gets an answer about *that*
  home — and the environment is consulted in exactly one place, `machine_root`,
  which `install-native-host` uses for its default root, so no manifest lands
  anywhere new. That is the native-host path only: the scanner's Windows *cache*
  directory still reads `%LOCALAPPDATA%`, because that cache is a known folder
  rather than a child of `$HOME`.

### Fixed

- `export` refuses to write through a symlink below `--out`. With `--force` on
  an `--out` holding a pre-existing component that pointed outside `--out`,
  archived content landed outside the directory the user named. A component that
  cannot be inspected also refuses the write: "unreadable" is not "absent".
- The dashboard's accepted connection no longer inherits the listener's
  non-blocking mode, and a narration failure is no longer fatal to a dashboard
  whose output is a socket.
- The `reload` dev cycle: repeatable unpacked reloads with normalized build
  numbers, a recoverable swap window, and cleanup failures reported instead of
  swallowed.

### Repository and release tooling

Nothing here is in the shipped binary.

- Commit messages, pull requests, issues, comments and docs must be English, and
  a `scripts/hooks/commit-msg` hook plus a CI job enforce it. The one exception
  is `apps/extension/locales/zh_CN.yml`.
- `scripts/relocate-citations.py` moves `file:line` citations across a merge
  mechanically, and refuses any citation whose new position is not forced, so a
  citation is never quietly pointed at the wrong line.
- `scripts/check-citation-drift.py` now also scans `contracts/`, and the drift
  self-test was repaired.
- `scripts/dev/reload-extension.sh` and its test were added to the gates.
