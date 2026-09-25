//! W120c · the body cache must never delete a file outside its own root.
//!
//! The cache's layout is `<root>/<pack id>/<offset>-<length>`, and a read that
//! finds a corrupt entry deletes it so the refetch can replace it. If the pack
//! directory — or the entry itself — is a symlink to somewhere else, following
//! it would make that delete land on a file the cache does not own. These tests
//! plant exactly those links and prove the file outside is untouched by every
//! path that could destroy it: a read's `get`, a store's rename, an eviction
//! pass, and `cache clear`.
//!
//! The mechanism is a symbolic link. On Windows, creating one through the
//! standard API needs a developer-mode flag or admin rights, so the fixture
//! cannot be built there; the guard the tests exercise is platform-independent
//! `lstat`, and the rest of the body-cache suite runs on every platform.

use chat_stasher::body_cache::{root_state, BodyCache, CacheKey, RootState, ENTRY_HEADER_LEN};
use rustic_core::Id;
use std::fs::{self, FileTimes, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A pack id that is a function of `n`, so two keys in one test cannot collide.
fn key(n: u8, offset: u32, length: u32) -> CacheKey {
    let hex = format!("{:02x}", n.wrapping_mul(7)).repeat(32);
    CacheKey::new(&hex.parse::<Id>().expect("hex id"), offset, length)
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// A directory outside the cache, holding one file named exactly like a cache
/// entry. `0-4` is what `key(_, 0, 4)` writes, so a link into here is the
/// strongest possible decoy.
fn outside_with_victim(base: &Path) -> PathBuf {
    let outside = base.join("outside");
    fs::create_dir_all(&outside).expect("mkdir outside");
    fs::write(outside.join("0-4"), b"a file chat-stasher did not write").expect("write victim");
    outside
}

/// Replace `<root>/<pack>/` with a symlink to `outside`, as an attacker or an
/// accident could leave it. The real pack directory (and the entry inside it)
/// is removed first, so only the link remains.
fn link_pack_dir_to(root: &Path, k: &CacheKey, outside: &Path) {
    let pack = k.path(root).parent().expect("pack dir").to_path_buf();
    fs::remove_dir_all(&pack).expect("remove the real pack dir");
    std::os::unix::fs::symlink(outside, &pack).expect("symlink the pack directory");
}

fn set_mtime(path: &Path, when: SystemTime) {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open entry to touch");
    file.set_times(FileTimes::new().set_modified(when))
        .expect("set mtime");
}

#[cfg(unix)]
#[test]
fn a_read_never_deletes_through_a_symlinked_pack_directory() {
    let dir = tempdir();
    let root = dir.path().join("body");
    let cache = BodyCache::new(root.clone(), 1 << 20);
    // The first store creates the marked root and a real pack directory.
    let k = key(1, 0, 4);
    cache.put(&k, b"aaaa");
    assert_eq!(root_state(&root), RootState::Cache);

    let outside = outside_with_victim(dir.path());
    link_pack_dir_to(&root, &k, &outside);

    // The lookup must be a miss — the link's target is not a cache entry — and
    // it must not delete the file the link reaches.
    assert_eq!(
        cache.get(&k),
        None,
        "the link's target is not a cache entry"
    );
    assert!(
        outside.join("0-4").exists(),
        "a read deleted a file outside the cache root"
    );
    // The refusal is reported, not silent.
    assert_eq!(cache.stats().corrupt, 1);
    assert_eq!(cache.stats().errors, 1);
}

#[cfg(unix)]
#[test]
fn a_read_leaves_a_symlinked_entry_alone() {
    let dir = tempdir();
    let root = dir.path().join("body");
    let cache = BodyCache::new(root.clone(), 1 << 20);
    let k = key(2, 0, 4);
    cache.put(&k, b"aaaa");
    let entry = k.path(&root);
    fs::remove_file(&entry).expect("remove the real entry");

    let outside = outside_with_victim(dir.path());
    std::os::unix::fs::symlink(outside.join("0-4"), &entry).expect("symlink the entry");

    assert_eq!(
        cache.get(&k),
        None,
        "a symlinked entry is not a cache entry"
    );
    assert!(
        outside.join("0-4").exists(),
        "a read deleted the target of a symlinked entry"
    );
    assert!(
        fs::symlink_metadata(&entry).is_ok(),
        "the entry was a symlink, not a regular file, and must be refused rather \
         than unlinked"
    );
}

#[cfg(unix)]
#[test]
fn a_store_never_writes_through_a_symlinked_pack_directory() {
    let dir = tempdir();
    let root = dir.path().join("body");
    let cache = BodyCache::new(root.clone(), 1 << 20);
    // The first store creates the marked root and a real pack directory.
    cache.put(&key(30, 0, 4), b"aaaa");
    let stored_before = cache.stats().stored;

    let outside = outside_with_victim(dir.path());
    let victim = outside.join("0-4");
    let before = fs::read(&victim).expect("read victim");

    // A symlink at the pack directory of a key not stored yet: a store would
    // create its temp file here and rename it over the outside file.
    let k = key(31, 0, 4);
    let pack = k.path(&root).parent().expect("pack").to_path_buf();
    std::os::unix::fs::symlink(&outside, &pack).expect("symlink pack dir");

    cache.put(&k, b"bbbb");

    assert_eq!(
        cache.stats().stored,
        stored_before,
        "a store must not land through a symlinked pack directory"
    );
    assert_eq!(
        cache.stats().errors,
        1,
        "the refusal is counted, not silent"
    );
    assert_eq!(
        fs::read(&victim).expect("read victim"),
        before,
        "a store changed a file outside the cache root"
    );
}

#[cfg(unix)]
#[test]
fn eviction_never_deletes_through_a_symlinked_pack_directory() {
    let dir = tempdir();
    // Room for three 100-byte payloads and their headers, not four.
    let quota = 3 * (ENTRY_HEADER_LEN as u64 + 100);
    let cache = BodyCache::new(dir.path().join("body"), quota);
    let keys = [key(10, 0, 100), key(11, 0, 100), key(12, 0, 100)];
    for (i, k) in keys.iter().enumerate() {
        cache.put(k, &[b'a' + i as u8; 100]);
        // Distinct, ordered mtimes: the LRU order must be the clock's, not the
        // directory's.
        set_mtime(
            &k.path(cache.root()),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64),
        );
    }

    // A pack directory the cache did not create: a symlink to an outside
    // directory, whose file is older than every entry so a pass that could see
    // it would delete it first.
    let outside = outside_with_victim(dir.path());
    set_mtime(&outside.join("0-4"), SystemTime::UNIX_EPOCH);
    let linked = key(13, 0, 100);
    let linked_pack = linked
        .path(cache.root())
        .parent()
        .expect("pack")
        .to_path_buf();
    std::os::unix::fs::symlink(&outside, &linked_pack).expect("symlink pack dir");

    // One entry more than the quota holds, which is what starts eviction.
    cache.put(&key(14, 0, 100), &[b'e'; 100]);

    assert!(
        outside.join("0-4").exists(),
        "eviction deleted a file outside the cache root"
    );
    assert!(
        cache.usage().expect("usage").bytes <= quota,
        "the quota must still be met by evicting the cache's own entries"
    );
    assert!(
        !keys[0].path(cache.root()).exists(),
        "the oldest entry of this cache is what the quota costs"
    );
}

#[cfg(unix)]
#[test]
fn clear_never_deletes_through_a_symlinked_pack_directory() {
    let dir = tempdir();
    let root = dir.path().join("body");
    let cache = BodyCache::new(root.clone(), 1 << 20);
    // One real entry, so the clear has something of its own to remove.
    cache.put(&key(20, 0, 4), b"aaaa");

    // A second pack-id name, a symlink to an outside directory.
    let outside = outside_with_victim(dir.path());
    let linked = key(21, 0, 4);
    let linked_pack = linked.path(&root).parent().expect("pack").to_path_buf();
    std::os::unix::fs::symlink(&outside, &linked_pack).expect("symlink pack dir");

    let removed = cache.clear().expect("clear a cache directory it created");

    assert!(
        outside.join("0-4").exists(),
        "`cache clear` deleted a file outside the cache root"
    );
    assert_eq!(
        removed.entries, 1,
        "only the cache's own entry may be removed"
    );
    assert!(
        removed.foreign_entries >= 1,
        "the symlinked directory must be reported as left alone"
    );
    assert_eq!(root_state(&root), RootState::Cache);
}
