//! Read-only helper behind `scripts/dev/prune-test-rustic-cache.sh` (W289).
//!
//! Prints, for every repository the user's real config declares — the local
//! single-destination repo and every `[destinations.<name>]` entry — the
//! repository id that rustic itself derives from that repository's config
//! file. The id is exactly the name of that repository's metadata-cache
//! directory (`Cache::new` pushes `id.to_hex()`), which is the fact the prune
//! script needs: the directories it may never delete are the ones whose names
//! appear here.
//!
//! Why an example exists for this at all: the repository's config file is
//! `ENCRYPTED` in the rustic format (`repofile.rs:23` — only key files are
//! not), so the id cannot be read out of `<repo>/config` by a text tool. The
//! only honest reader is the same code that opens the repository, so the
//! helper opens each repository the way the CLI would — and then it must be
//! held to the two rules the prune script's safety rests on:
//!
//! * **never write to a repository.** Opening with `no_cache = true` means
//!   `open_raw` never creates a metadata-cache directory, and `open` itself
//!   reads the config file and nothing else — keys come from the local key
//!   file, snapshots and packs are not touched.
//! * **partial answers say so.** A repository that cannot be opened (no
//!   network for a remote backend, a missing credential in this shell) is
//!   reported as `error`, never silently omitted: a keep-set with a hole in
//!   it is precisely what the prune script must refuse to act on.
//!
//! Output format, one line per repository, so the script can parse it without
//! a TOML reader of its own:
//!
//! ```text
//! id     <label> <64-hex repository id>     # derived, kept
//! absent <label>                            # declared, not initialised — no cache can exist
//! error  <label> <reason, one line>          # declared, could not be read — keep-set is incomplete
//! end                                       # terminator; the script refuses to miss it
//! ```
//!
//! `label` is `local` for the single-destination repository, else the
//! destination's name in config order. A config file that cannot be loaded at
//! all prints `config-error <reason>` and exits 1: with no config there is no
//! keep-set, and a prune without a keep-set is not a tool, it is a hazard.
//!
//! Not a product surface: it is an example, built only when asked for
//! (`cargo run -p chat-stasher --example repo-config-id --quiet`), and when
//! the config fails to load the honest exit is 1 so no caller can mistake
//! half a keep-set for a whole one. That is a dev-helper's exit discipline,
//! not the product's exit-code contract (3/1/2), and the difference is stated
//! here so nobody reads one as the other.

use anyhow::Context;
use chat_stasher::config::Config;
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::{Credentials, Repository};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() {
    let report = probe();
    let code = match report {
        Ok(()) => 0,
        Err(reason) => {
            println!("config-error {}", one_line(&reason));
            println!("end");
            1
        }
    };
    std::process::exit(code);
}

/// One line kept from an error chain: multi-line error renders are how paths
/// and endpoints leak into reports wholesale, when one quoted line is all a
/// human needs to recognize the failure.
fn one_line(err: &anyhow::Error) -> String {
    let text = err.to_string();
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(160).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Same shape as [`main`] but as a `Result` so the two outcomes stay visible
/// in the types: a produced report (`Ok`) can still name unreadable
/// repositories; an unloadable config (`Err`) produces no report at all.
fn probe() -> anyhow::Result<()> {
    let config = Config::load().context("read the chat-stasher config")?;
    let data_root = chat_stasher::config::default_data_root();

    // The single-destination repository, resolved exactly like
    // `main.rs::store_config_from`: `rustic_repo`/`rustic_key_file` with the
    // data-dir defaults behind them.
    let local_repo = config
        .rustic_repo
        .clone()
        .unwrap_or_else(|| data_root.join("repo").to_string_lossy().into_owned());
    let local_key = config
        .rustic_key_file
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| data_root.join("masterkey.json"));
    report_one(
        "local",
        &StoreConfig {
            repo_root: local_repo,
            key_file: local_key,
            connections: 1,
            // Cache knobs are forced off, not copied from the config: this
            // helper's guarantee to the prune script is that reading ids
            // writes nothing anywhere, and honouring a configured
            // `rustic_cache_dir` would only move where the write happens.
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: true,
        },
    );

    // Destinations, in config (BTreeMap) order, resolved like
    // `main.rs::resolve_store_config_checked`: per-destination repo and key
    // with the per-name key default. `repo` is not optional there — a
    // destination without one is refused by the CLI — so here `None` is an
    // `error` line, the same state a caller farther up would have reached.
    for (name, entry) in &config.destinations {
        let Some(repo_root) = entry.repo.clone() else {
            println!("error {name} destination has no repo set");
            continue;
        };
        let key_file = entry
            .key_file
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| data_root.join(format!("masterkey-{name}.json")));
        report_one(
            name,
            &StoreConfig {
                repo_root,
                key_file,
                connections: entry.connections.unwrap_or(1),
                options: entry.options.clone(),
                cache_dir: None,
                no_cache: true,
            },
        );
    }
    println!("end");
    Ok(())
}

/// Read-only open of one repository: `repository_exists` lists the config type
/// without a key, and `open` reads exactly one config file with it.
fn report_one(label: &str, cfg: &StoreConfig) {
    let mk = match store::load_key_file(cfg) {
        Ok(mk) => mk,
        Err(err) => {
            println!("error {label} {}", one_line(&err));
            return;
        }
    };
    let store = BackupStore::for_metadata_query(cfg.clone());
    let exists = match store.repository_exists() {
        Ok(exists) => exists,
        Err(err) => {
            println!("error {label} {}", one_line(&err));
            return;
        }
    };
    if !exists {
        println!("absent {label}");
        return;
    }
    let id = match open_id(&store, cfg, &mk) {
        Ok(id) => id,
        Err(err) => {
            println!("error {label} {}", one_line(&err));
            return;
        }
    };
    println!("id {label} {id}");
}

/// `Repository::open` resolves the same way every product read does; the id
/// printed is the one the repository's own config file carries
/// (`repo.config().id`), which is the name rustic's cache directory for it
/// will bear. No `~` expansion happens here because every path in a loaded
/// config was already expanded by `expand_all_paths` during `Config::load`.
fn open_id(
    store: &BackupStore,
    cfg: &StoreConfig,
    mk: &rustic_core::repofile::MasterKey,
) -> anyhow::Result<String> {
    let backends = store.backends()?;
    let repo = Repository::new(&cfg.repository_options(), &backends)?
        .open(&Credentials::Masterkey(mk.clone()))?;
    Ok(repo.config().id.to_hex().to_string())
}
