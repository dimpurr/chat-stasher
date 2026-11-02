//! W1079: a listing that stops part-way must be an error, never a shorter list.
//!
//! Production incident (2026-11-02, storage box, `verify --level l1`): the
//! recursive listing of `data/` returned only the first 124 (and, on a rerun,
//! 113) of 256 prefix directories, as a successful answer, and L1 reported every
//! pack of the other directories as "referenced by the index but not present".
//! The cause is in `rustic_backend`'s opendal lister, which logs a listing
//! error and returns what it had as `Ok`. The SFTP family now answers listings
//! from the writer's own IO, where an error stays an error.
//!
//! The real SFTP server cannot run in the gate, so the lister that loses its
//! tail is a stand-in with the same observable shape (`Ok` of the first N
//! entries of the real listing), and the writer's IO is the real
//! [`OperatorIo`] over the local filesystem.

use bytes::Bytes;
use chat_stasher::atomic_pack::{AtomicPackBackend, FileEntry, OperatorIo, PackIo, RetryPolicy};
use rustic_core::{FileType, Id, ReadBackend, RusticResult, WriteBackend};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const PREFIXES: usize = 256;
const CUT_AFTER: usize = 124;

fn pack_name(prefix: usize) -> String {
    format!("{prefix:02x}{:062x}", prefix + 1)
}

/// A destination shaped like production: 256 prefix directories under `data/`,
/// one pack each, plus tmp debris and a stray that are not packs.
fn fixture(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for prefix in 0..PREFIXES {
        let name = pack_name(prefix);
        let dir = root.join("data").join(&name[..2]);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(&name), vec![7_u8; 10 + prefix]).unwrap();
        names.insert(name);
    }
    let dir = root.join("data").join("00");
    fs::write(
        dir.join(format!("{}.tmp-5-0011223344556677", pack_name(0))),
        b"debris",
    )
    .unwrap();
    fs::write(dir.join("notes.txt"), b"stray").unwrap();
    names
}

/// `rustic_backend`'s lister as the incident showed it: the listing error is
/// dropped and the entries collected so far come back as a successful answer.
struct LosesItsTail {
    complete: Vec<(Id, u32)>,
}

impl ReadBackend for LosesItsTail {
    fn location(&self) -> String {
        "loses-its-tail".into()
    }
    fn list_with_size(&self, _: FileType) -> RusticResult<Vec<(Id, u32)>> {
        Ok(self.complete[..CUT_AFTER].to_vec())
    }
    fn read_full(&self, _: FileType, _: &Id) -> RusticResult<Bytes> {
        unreachable!("a listing never reads a file")
    }
    fn read_partial(&self, _: FileType, _: &Id, _: bool, _: u32, _: u32) -> RusticResult<Bytes> {
        unreachable!("a listing never reads a file")
    }
    fn warmup_path(&self, _: FileType, _: &Id) -> String {
        String::new()
    }
}

impl WriteBackend for LosesItsTail {
    fn write_bytes(&self, _: FileType, _: &Id, _: bool, _: Bytes) -> RusticResult<()> {
        unreachable!("a listing never writes")
    }
    fn remove(&self, _: FileType, _: &Id, _: bool) -> RusticResult<()> {
        unreachable!("a listing never deletes")
    }
}

fn truncating_inner(names: &BTreeSet<String>) -> Arc<LosesItsTail> {
    let complete = names
        .iter()
        .enumerate()
        .map(|(n, name)| (Id::parse_some(name, FileType::Pack).unwrap(), 10 + n as u32))
        .collect();
    Arc::new(LosesItsTail { complete })
}

fn real_io(root: &Path) -> Box<dyn Fn() -> anyhow::Result<Arc<dyn PackIo>> + Send + Sync> {
    let mut options = BTreeMap::new();
    options.insert("root".to_string(), root.to_str().unwrap().to_string());
    Box::new(move || {
        let io: Arc<dyn PackIo> = Arc::new(OperatorIo::open("fs", options.clone())?);
        Ok(io)
    })
}

fn policy() -> RetryPolicy {
    RetryPolicy {
        attempts: 1,
        backoff: Duration::from_millis(1),
    }
}

fn hexes(listed: &[(Id, u32)]) -> BTreeSet<String> {
    listed
        .iter()
        .map(|(id, _)| id.to_hex().to_string())
        .collect()
}

#[test]
fn a_listing_that_lost_its_tail_in_the_inner_lister_is_still_complete() {
    let dir = tempfile::tempdir().unwrap();
    let names = fixture(dir.path());
    let inner = truncating_inner(&names);
    assert_eq!(
        hexes(&inner.list_with_size(FileType::Pack).unwrap()).len(),
        CUT_AFTER,
        "the stand-in must reproduce the incident's short answer"
    );

    let backend = AtomicPackBackend::around_listing(inner, real_io(dir.path()), policy(), true);
    let listed = backend.list_with_size(FileType::Pack).unwrap();

    assert_eq!(hexes(&listed), names, "all {PREFIXES} packs, none dropped");
    assert_eq!(listed.len(), PREFIXES, "debris and strays are not packs");
    let size_of: BTreeMap<String, u32> = listed
        .iter()
        .map(|(id, size)| (id.to_hex().to_string(), *size))
        .collect();
    assert_eq!(size_of[&pack_name(3)], 13, "sizes come from the listing");
    // `list` is the default over `list_with_size`: the same answer.
    assert_eq!(backend.list(FileType::Pack).unwrap().len(), PREFIXES);
}

/// A writer IO whose listing fails: the connection died part-way.
struct ListingFails;

impl PackIo for ListingFails {
    fn write(&self, _: &str, _: Bytes) -> anyhow::Result<()> {
        unreachable!()
    }
    fn read(&self, _: &str) -> anyhow::Result<Bytes> {
        unreachable!()
    }
    fn exists(&self, _: &str) -> anyhow::Result<bool> {
        unreachable!()
    }
    fn rename(&self, _: &str, _: &str) -> anyhow::Result<()> {
        unreachable!()
    }
    fn delete(&self, _: &str) -> anyhow::Result<()> {
        unreachable!()
    }
    fn list_files(&self, _: &str) -> anyhow::Result<Vec<FileEntry>> {
        Err(anyhow::anyhow!("the connection to the destination failed"))
    }
}

#[test]
fn a_listing_that_fails_is_an_error_and_never_the_inner_lists_short_answer() {
    let dir = tempfile::tempdir().unwrap();
    let names = fixture(dir.path());
    let backend = AtomicPackBackend::around_listing(
        truncating_inner(&names),
        Box::new(|| {
            let io: Arc<dyn PackIo> = Arc::new(ListingFails);
            Ok(io)
        }),
        policy(),
        true,
    );
    for tpe in [
        FileType::Pack,
        FileType::Index,
        FileType::Snapshot,
        FileType::Key,
    ] {
        let error = backend
            .list_with_size(tpe)
            .expect_err("a failed listing must not come back as a list");
        assert!(
            format!("{error}").contains("did not finish"),
            "the error says the listing did not finish: {error}"
        );
    }
}

#[test]
fn the_config_file_and_the_other_families_keep_the_inner_listing() {
    let dir = tempfile::tempdir().unwrap();
    let names = fixture(dir.path());
    // `config` has no directory to walk: always the inner answer.
    let covering = AtomicPackBackend::around_listing(
        truncating_inner(&names),
        Box::new(|| unreachable!("config must not open the writer")),
        policy(),
        true,
    );
    assert_eq!(
        covering.list_with_size(FileType::Config).unwrap().len(),
        CUT_AFTER
    );
    // A family that is not asked for authoritative listings is unchanged and
    // never opens a second connection to list.
    let plain = AtomicPackBackend::around_covering(
        truncating_inner(&names),
        Box::new(|| unreachable!("a plain wrapper must not open the writer to list")),
        policy(),
        true,
    );
    assert_eq!(
        plain.list_with_size(FileType::Pack).unwrap().len(),
        CUT_AFTER
    );
}
