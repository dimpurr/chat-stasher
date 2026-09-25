# Destinations

A **destination** is a place your archive lives. This page sets up each kind:

| Destination | Status | Best for |
|---|---|---|
| [Cloudflare R2](#cloudflare-r2) | ✅ Verified end to end (2026-09) | An off-site copy with a free tier |
| [SFTP](#sftp) (for example, a Hetzner Storage Box) | ✅ In daily use | An off-site copy on a server you rent |
| [A local folder or disk](#a-local-folder-or-disk) | ✅ Supported | One machine, or a second copy on an external disk |

Other backends: a **rustic REST server** (`repo = "rest:…"`) is accepted by the config but has not been verified. Other **S3-compatible services** use the same options as R2, but only R2 has been tested.

## How destinations work

Read this once. It applies to every kind.

- **Each destination is a full, separate copy, with its own key.** Everything is encrypted before it is sent, and a destination's key file (`key_file`) is created the first time that destination is initialised. Back up every key file. A lost key cannot be recovered.
- **Destinations are declared by name** in `~/.config/chat-stasher/config.toml`, under `[destinations.<name>]`. Once you declare one, every command that reads or writes an archive needs `--destination <name>` (or an explicit `--repo`). There is deliberately no default, so you always know which copy you are touching. The one exception is `ui`, which opens your only declared destination when there is just one.
- **Coming from [start.md](start.md)? Declare your local archive too, and re-run `schedule`.** Declaring your first named destination changes two things. Your existing local archive no longer has a name, so give it one: the paths below are the defaults it already uses. And the timer you installed passes no `--destination`, so it will stop working until you run `schedule` again with `--destination`.

  ```toml
  [destinations.laptop]
  repo = "~/.local/share/chat-stasher/repo"
  key_file = "~/.local/share/chat-stasher/masterkey.json"
  ```
- **`dest-init` sets up a new destination, once.** It first re-collects this machine's sessions from their source files. Then it copies in whatever your other destinations still hold **for this machine** that the machine itself no longer has, and pushes the result. It seeds this machine's part of the archive only. Other machines' history arrives when those machines push to the same destination.
- **`run-once --destination <name>`** keeps a destination current. The timer that `schedule` writes pushes to **one** destination, the one you pass with `--destination`. There is no built-in way yet to schedule a second destination; run `run-once --destination <other>` yourself, or add your own timer for it.
- **`doctor` dials each declared destination once, read-only.** It reports each one as reached, not reached (with the reason), or not configured. It creates nothing.
- **Unknown is not empty.** A destination that cannot be read makes a command exit `3`, meaning "did not finish". It never reports the destination as empty.

To check an archive's integrity at any time:

```sh
chat-stasher verify --destination <name> --level l1   # structure only, cheap
chat-stasher verify --destination <name> --level l2   # downloads and re-hashes everything
```

## Cloudflare R2

R2 is S3-compatible object storage. Its free tier covers 10 GB-month of Standard storage, which an archive of text conversations reaches very slowly. [Cloudflare's pricing page](https://developers.cloudflare.com/r2/pricing/) is the authority and its terms change: it said 10 GB-month when this page was checked, on 2026-09-25, against a page last updated 2026-08-07.

chat-stasher reaches R2 through its S3 backend (`opendal:s3`). This is the configuration that was exercised end to end: one machine's partition seeded, an ordinary push, sessions read back out, and both integrity levels reporting no findings.

### 1. Create a bucket and a token

1. In the Cloudflare dashboard, open **R2 Object Storage** and create a bucket, for example `chat-stasher`.
2. Create an **R2 API token** with **Object Read & Write** permission, **limited to that one bucket**. chat-stasher never needs to list your other buckets, so a bucket-scoped token is enough.
3. Note three values: your **Account ID** (it appears in the endpoint URL), the token's **Access Key ID**, and its **Secret Access Key**.

### 2. Declare the destination

Add this to `~/.config/chat-stasher/config.toml`, replacing the `<…>` placeholders:

```toml
[destinations.r2]
repo = "opendal:s3"
key_file = "~/.local/share/chat-stasher/masterkey-r2.json"

[destinations.r2.options]
endpoint = "https://<your-account-id>.r2.cloudflarestorage.com"
bucket = "<your-bucket>"
region = "auto"
access_key_id = "<your-access-key-id>"
secret_access_key = "<your-secret-access-key>"
disable_config_load = "true"
disable_ec2_metadata = "true"
```

Why each line is there:

- `region = "auto"` is **required**. The backend does not fill it in, and fails without it.
- `disable_config_load = "true"` stops the backend from also picking up AWS credentials from your environment or `~/.aws`. `disable_ec2_metadata = "true"` is a second, separate switch, and it is the one that keeps the machine's instance metadata service out of the chain. Without it, a missing or mistyped credential can authenticate as the machine's role.
- `key_file` should be a new file. It is created on the first `dest-init`, and it is this destination's key. Back it up.
- To keep the repository under a prefix inside the bucket rather than at its root, add `root = "/chat-stasher/v1"`.

The two credentials in this file are plaintext, so make the file readable only by you:

```sh
chmod 600 ~/.config/chat-stasher/config.toml
```

### Keeping the secret out of the config file (`env:`)

Any option value written as `env:NAME` is read from the environment variable `NAME` when the config loads:

```toml
access_key_id = "env:CHAT_STASHER_R2_ACCESS_KEY_ID"
secret_access_key = "env:CHAT_STASHER_R2_SECRET_ACCESS_KEY"
```

If the variable is missing or empty, chat-stasher drops that option and prints a warning. The warning names the option and the variable, never the value. The trade-off is where the variable has to exist:

- **Every process that uses this destination needs the variable**, including a timer. A launchd or systemd job does not see the environment of your shell, and `schedule` does not add variables to the timer file yet. A scheduled command that cannot resolve the credentials cannot reach R2. For the weekly stage clean-up (`reclaim-stage`), this fails safe: nothing is deleted. But nothing is reclaimed either, and the only sign of it is in that job's log.
- **An env file is not exported just by sourcing it.** If you keep the variables in a file without `export` lines, load it with `set -a; . <file>; set +a` in the same shell that runs the command.

If you want the timer to reach R2 today, the plaintext form above with `chmod 600` is the dependable one.

### 3. Initialise it

```sh
chat-stasher dest-init --destination r2 --stage ~/stash/chat-stasher/stage
```

This seeds R2 with this machine's history (see [How destinations work](#how-destinations-work)), and it refuses if the address or the credentials are wrong, before anything is written. Then push to it hourly with `schedule --destination r2`.

### Pitfalls

| You see | What it means |
|---|---|
| `failed to load signing credential` | The credentials did not reach the backend. With `env:`, the variable was not set in this process. |
| `Path `config` does not exist`, from a host you expected to reach | The first sentence can mislead. Read the `Caused by:` lines at the end: a DNS or connection error there means the endpoint was **never reached**, not that the bucket is empty. The command exits `3`. |
| `region is missing` | Add `region = "auto"`. |
| Errors after adding options such as `checksum_algorithm`, `default_acl` or `enable_request_payer` | R2 does not implement those S3 features. Leave them unset. |

## SFTP

Any server you can reach over SSH works. A Hetzner Storage Box is what has been used daily. chat-stasher runs your system's `ssh` client, so your SSH keys and `~/.ssh/config` apply.

### 1. Declare the destination

```toml
[destinations.storagebox]
repo = "opendal:sftp"
key_file = "~/.local/share/chat-stasher/masterkey-storagebox.json"

[destinations.storagebox.options]
endpoint = "ssh://<your-host>:<your-port>"
user = "<your-user>"
key = "~/.ssh/id_ed25519"
root = "<folder on the server>"
```

The options table is passed to the SFTP backend as written.

### 2. Trust the server, once, yourself

The first time chat-stasher meets a server it has no record of, SSH refuses the connection, and **that refusal is deliberate**. It is the one moment you can confirm "this really is my server" before anything is sent.

1. Run `dest-init`. It stops with exit code `3` and prints the fingerprints the server presented, and nothing is written:

   ```sh
   chat-stasher dest-init --destination storagebox --stage ~/stash/chat-stasher/stage
   ```
2. **Compare those fingerprints with the ones your provider publishes.** For example, Hetzner lists them in the Storage Box overview. `ssh-keyscan` output alone proves nothing, because someone on the network path can substitute their own key. **If they do not match, stop.**
3. If they match, record them and continue:

   ```sh
   chat-stasher dest-init --destination storagebox --stage ~/stash/chat-stasher/stage --trust-host
   ```

   `--trust-host` is the only thing in chat-stasher that writes to `~/.ssh/known_hosts`, and it prints each record it adds. A scheduled run never trusts a new host by itself: it stops instead.

A server whose key has **changed** since you trusted it is refused, and no flag gets past that. It can mean someone is impersonating your server. Find out why the key changed before you edit `known_hosts`.

**Optional: `known_hosts_strategy`.** Leaving it out means `strict`, the behaviour above. `add` accepts new hosts automatically, and `accept` accepts any key, including a changed one. Both move the trust decision away from you. Choose them deliberately, if at all.

## A local folder or disk

This is the default. With no destination declared, `run-once` archives to `~/.local/share/chat-stasher/repo`, and nothing needs configuring.

A local archive is the right first step, because it stops tools from deleting their own history. It is not an off-site backup, though: it is lost with the disk it is on. It also belongs to **one machine**. Other computers cannot push to it, unless the folder is on storage they all mount.

To keep a second copy on an external disk, declare it:

```toml
[destinations.external]
repo = "/Volumes/<your-disk>/chat-stasher/repo"
key_file = "~/.local/share/chat-stasher/masterkey-external.json"
```

Keep the key file **off** that disk, as above. Anyone who finds a disk that holds both the archive and its key can read everything in it. Then initialise it:

```sh
chat-stasher dest-init --destination external --stage ~/stash/chat-stasher/stage
```

For the strongest local setup, put the archive on an encrypted volume. The provider then sees nothing at all, because there is no provider.
