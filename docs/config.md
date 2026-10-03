# Configuration reference

<!-- RELEASE GATE: the file:/env-file:/keychain: credential forms and the full-text index row are merged after 0.5.0-rc.2. -->

chat-stasher reads one file: `~/.config/chat-stasher/config.toml` (or `$XDG_CONFIG_HOME/chat-stasher/config.toml` when that variable is set). **Every setting is optional.** With no file at all, chat-stasher uses the defaults below, and that is the normal state on a first run.

`chat-stasher init` writes the file with every setting present as a comment, and never overwrites a file that exists.

## How the file is read

| Rule | Detail |
|---|---|
| Errors stop the command | A file that exists must be valid. A typo stops every command that reads it, naming the file, the line and the reason. To fall back to the defaults, move the file aside instead of leaving it broken. |
| `~` in paths | A leading `~/` is your home folder, and `~` alone is too. On Windows, `~\` works as well. `~username` is refused. |
| No silent substitution | A path that cannot be resolved stops the command and names the setting. It is never replaced with a default, and a folder named `~` is never created. |
| Windows paths | Write them in single quotes (`'C:\Users\me\…'`), because `\` is an escape character inside double quotes. A path pasted into double quotes still loads, with a warning. |
| Command line wins | A flag such as `--repo`, `--key-file` or `--connections` overrides the matching setting for that one command. |
| Environment override for the metadata cache | `CHAT_STASHER_RUSTIC_CACHE_DIR` names the metadata cache root for this run and wins over the file's `rustic_cache_dir` (a destination's own `cache_dir` still wins over it). It exists for a run whose cache must not land in the user's cache directory — a CI job, or a sandbox with a read-only home — and it is the only spelling that works on Windows, where rustic's default root comes from the Known Folder API and no environment variable moves it. An empty value counts as unset. |

## Top-level settings

| Key | Default | Meaning |
|---|---|---|
| `archive_root` | unset | A path for archived snapshots. `doctor` is the only command that reads it today: when no `rustic_repo` is set, it is the label that command uses for the single-destination case. The archive itself is `rustic_repo`. |
| `rustic_repo` | `~/.local/share/chat-stasher/repo` | The local archive used when no destination is declared. |
| `rustic_key_file` | `~/.local/share/chat-stasher/masterkey.json` | Its key file, created with the archive. This key opens the **local** archive only: each destination has a key file of its own, defaulting to `~/.local/share/chat-stasher/masterkey-<destination>.json` and set with that destination's `key_file`. |
| `rustic_connections` | `4` | Concurrency for archive reads and writes. Maximum `10`. Raising it has not been measured to help. |
| `rustic_cache_dir` | the platform cache folder, under `rustic` | Where archive **metadata** is cached. Never holds conversation data. |
| `rustic_no_cache` | `false` | Turn that metadata cache off. Nothing is lost; each command re-reads metadata instead. |
| `backup_interval_secs` | `3600` | How often the timer runs. Read when the timer is rendered, so run `schedule install` again after changing it. |
| `push_only_if_changed` | `true` | Skip the push when nothing changed, and tell `dest-init` not to publish a snapshot the destination already holds. With `false`, an hourly `run-once` and every `dest-init` write a snapshot even when nothing changed (each snapshot costs a little metadata). An explicit `push` always records the stage as it stands. |
| `machine` | unset | Pin this machine's partition name in the archive. New installs leave it unset: a random identity is generated on the first run. Set it only to keep writing to a partition an older install created. |
| `claude_projects_dir` | `~/.claude/projects` | Where Claude Code keeps sessions. |
| `codex_sessions_dir` | `~/.codex/sessions` | Where Codex CLI keeps sessions. |

Local paths follow `XDG_DATA_HOME` when it is set.

## `[harness_roots]`

Tells the scanner where a tool keeps its sessions, when that is not where the built-in registry looks, or when the registry has no verified path for your system. Keys are tool ids:

| Id | Tool | | Id | Tool |
|---|---|---|---|---|
| `claude-code` | Claude Code | | `github-copilot-cli` | GitHub Copilot CLI |
| `codex` | OpenAI Codex CLI | | `aider` | aider |
| `gemini-cli` | Gemini CLI | | `crush` | crush |
| `opencode` | opencode | | `zed` | Zed |
| `cursor` | Cursor | | `continue` | Continue |
| `grok` | Grok (xAI CLI) | | `kimi-code` | Kimi Code |
| `grok-bot` | Grok Bot desktop app | | `hermes-agent` | Hermes Agent |

```toml
[harness_roots]
cursor = "~/.config/Cursor/User/globalStorage/state.vscdb"
opencode = "~/.local/share/opencode/opencode.db"
```

- For a tool that stores one database file, give the file. For a tool that stores a folder of sessions, give the folder.
- `grok-bot` accepts the app-support directory; its reader locates `sand-client-persistence` below it, decodes the app's base32-named state blobs, and archives observed replica rows as partial data with sequence gaps recorded (positions before the first observed row included). It is available on macOS only. A row the app rewrites in place is archived as an additional variant of that sequence, never as a replacement.
- `hermes-agent` accepts the `state.db` file. Its legacy `~/.hermes/sessions` compatibility sources are scanned when no root is set here; a root you set is the whole source, and the default home paths are not consulted around it.
- A path you write here is always looked at, even where the registry has no verified path for your system.
- A path that does not exist is reported as *unknown*, never as "0 sessions".

[support.md](support.md) lists the path the registry uses for each tool and system.

## `[destinations.<name>]`

One table per place a copy of your archive lives. Each is a **full copy with its own key**, never a share of one. Setup for each kind is in [destinations.md](destinations.md).

> [!IMPORTANT]
> Once any destination is declared, every command that reads or writes an archive needs `--destination <name>` (or `--repo`). There is no default. The dashboard is the one exception: see [`[native_host]`](#native_host).

| Key | Default | Meaning |
|---|---|---|
| `repo` | the top-level default | Where the archive is. A local path, `opendal:s3`, `opendal:sftp`, or `rest:…` (accepted, not verified). |
| `key_file` | the top-level default | This destination's key file. Created on its first `dest-init`. Use a different file for each destination, and back each one up. |
| `connections` | `4` | Concurrency for this destination. |
| `cache_dir` | the platform default | This destination's metadata cache folder. |
| `no_cache` | `false` | Turn this destination's metadata cache off. |

### `[destinations.<name>.options]`

Passed to the storage backend as written. The keys depend on the kind:

| Kind | Keys |
|---|---|
| `opendal:s3` (R2 and other S3-compatible services) | `endpoint`, `bucket`, `region` (**required**; `"auto"` for R2), `access_key_id`, `secret_access_key`, `disable_config_load = "true"` (recommended: ignore AWS credentials elsewhere on the machine), `root` (optional folder prefix) |
| `opendal:sftp` | `endpoint` (`ssh://<host>:<port>`), `user`, `key` (a private key path; optional with an ssh agent), `root`, `known_hosts_strategy` (optional: `strict` by default, `add`, or `accept`) |

Leave S3 features R2 does not implement, such as `checksum_algorithm`, `default_acl` and `enable_request_payer`, unset.

### Credentials in options

Any option value can be a **reference** instead of the secret itself:

| Form | Read from | When it cannot be read |
|---|---|---|
| a literal value | the config file itself. `chmod 600` the file. | n/a |
| `env:NAME` | the environment variable `NAME` of the running process | The option is **dropped** with a warning naming the option and variable. The command then fails to authenticate. |
| `file:PATH` | the whole file, with one trailing newline removed | The config is **refused**, naming the option and the path. |
| `env-file:PATH:NAME` | the `NAME=` line of a dotenv-style file. `#` comments, an `export ` prefix and matching quotes are accepted. | Refused, as above. |
| `keychain:ACCOUNT` / `keychain:SERVICE:ACCOUNT` | macOS only: a generic password in the login keychain. `SERVICE` defaults to `chat-stasher`. | Refused, as above. |

Errors name the reference, never the secret. A timer does not see your shell's environment, so use `file:`, `env-file:` or `keychain:` for any destination a timer pushes to. See [schedule.md → Credentials a timer can reach](schedule.md#credentials-a-timer-can-reach).

### Example

```toml
[destinations.laptop]
repo = "~/.local/share/chat-stasher/repo"
key_file = "~/.local/share/chat-stasher/masterkey.json"

[destinations.r2]
repo = "opendal:s3"
key_file = "~/.local/share/chat-stasher/masterkey-r2.json"

[destinations.r2.options]
endpoint = "https://<account-id>.r2.cloudflarestorage.com"
bucket = "<bucket>"
region = "auto"
access_key_id = "file:~/.config/chat-stasher/r2-access-key-id"
secret_access_key = "keychain:r2"
disable_config_load = "true"
```

## `[native_host]`

Settings for the browser host. Both keys are optional.

| Key | Written by | Meaning |
|---|---|---|
| `stage` | `install-native-host --stage <dir>` | The **absolute** path of the stage the host delivers into. The host never creates it. If it is missing, deliveries are refused and captures wait in the extension. |
| `destination` | you, by hand | The destination the popup's **Open dashboard** opens, and the one `chat-stasher ui` opens when several are declared and none is named. Must be a declared destination. Without it, **Open dashboard** does not work, even with only one destination declared. |

```toml
[native_host]
stage = "/Users/me/stash/chat-stasher/stage"
destination = "r2"
```

`install-native-host` edits only its own key, and keeps the file's comments and everything else in it.

## `[cache]`

This machine's **body cache**: conversations you have opened, kept as the destination's own encrypted bytes, so reading them again is fast. Nothing is decrypted to store it, and every entry is re-checked before use. One quota covers every destination.

| Key | Default | Meaning |
|---|---|---|
| `max_bytes` | `"2GiB"` | The quota. Plain bytes or a unit: `"50GB"` is 50 × 10⁹, `"50GiB"` is 50 × 2³⁰. `0` turns the cache off. |
| `dir` | the platform cache folder (`~/Library/Caches/chat-stasher/body` on macOS) | Where entries live. |

- The least recently used entries are removed when the quota is reached.
- A session larger than a tenth of the quota is read without being stored.
- `verify`, `export`, `dest-init`, `push` and `read --all-machines` never use the cache, in either direction.
- A `[cache]` section with an unreadable value turns the cache **off** instead of guessing, and `doctor` says why.
- `chat-stasher cache` shows what it holds, and `chat-stasher cache clear` empties it.

## Files chat-stasher keeps outside the config

| What | Where | Safe to delete? |
|---|---|---|
| Local archive and key | `~/.local/share/chat-stasher/repo`, `masterkey.json` | **No.** This is your archive. |
| Machine identity | `~/.local/share/chat-stasher/machine-identity` | No. A new one starts a new partition. |
| Collection state | `~/.local/share/chat-stasher/state/` | Not recommended. |
| Stage | the folder you chose | Only through `reclaim-stage`. |
| Metadata cache | the platform cache folder, under `rustic` | Yes. |
| Body cache | see `[cache]` | Yes: `cache clear`. |
| Full-text index | the platform cache folder, under `chat-stasher/fts`, one per destination. **Plaintext.** | Yes: `index clear --destination <name>`. |
| Timer logs (macOS) | `~/Library/Logs/chat-stasher/` | Yes. |
