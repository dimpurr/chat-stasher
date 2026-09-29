//! Reading a pack's own header, and verifying every blob that header declares.
//!
//! A pack is `blob, blob, …, header, header-length`: the blobs are the
//! ciphertext of the archived content, the header is one more AEAD ciphertext
//! naming what the pack holds — for every blob its id, its type, and its offset
//! and length inside the pack — and the last four bytes are that header's own
//! length, unencrypted (`rustic_core` `src/repofile/packfile.rs:90-129,228-250`
//! for the entry layout and the offset accumulation, `src/blob/packer.rs:633-650`
//! for the shape a writer produces).
//!
//! Three checks make a pack trustworthy, and each covers something the one
//! before it cannot:
//!
//! 1. **The file reads back as the id it is stored under.** rustic names a pack
//!    by hashing the bytes it has just written (`src/blob/packer.rs:762`) and
//!    its own `check --read-data` recomputes exactly that
//!    (`src/commands/check.rs:736`), so this pins every byte of the file —
//!    including the header and the length field — to its name. It is what a
//!    pack that was damaged and left alone fails on.
//! 2. **The header decrypts with the repository key.** The header is AEAD
//!    ciphertext under the same key as the blobs, so a MAC failure means those
//!    bytes were not written by a holder of this repository's key; what comes
//!    out is what the pack *claims* to hold. This is what a pack that was
//!    damaged *and renamed to its new hash* is caught by next.
//! 3. **Every blob in the header decrypts, decompresses to the length the
//!    header gives it, and hashes to the id the header names.** This is the
//!    check the dedup entry rests on: a pack whose ciphertext was damaged in
//!    place, then renamed to the hash of its new bytes, passes 1 and 2 and
//!    fails here — which is the whole reason this module exists, because an
//!    index entry taken from it would let a later backup skip uploading content
//!    no reader can decrypt.
//!
//! The three together are what `rustic_core`'s own `check_pack`
//! (`src/commands/check.rs:718-813`) does for a pack an index file names. Why
//! this file calls the AEAD itself instead of reusing that code is recorded
//! once, in [`decrypt`]'s comment: the traits it would call through
//! (`DecryptReadBackend`, `CryptoKey`) are `pub(crate)` in the pinned version,
//! so the only reachable half of rustic's implementation is its *key*, which
//! this module takes from `MasterKey` exactly where rustic takes it. The pack
//! layout, the header parse (`HeaderEntry`, rustic's own type and its own
//! `binrw` reader) and the blob-id comparison (`Id::blob_matches_reader`,
//! rustic's own hash) are all reached through the public API.

use aes256ctr_poly1305aes::{aead::Aead, Aes256CtrPoly1305Aes, Key as AeadKey, Nonce};
use binrw::BinRead;
use rustic_core::repofile::{HeaderEntry, MasterKey};
use rustic_core::{FileType, Id, ReadBackend, WriteBackend};
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::num::NonZeroU32;
use std::sync::Arc;

/// The unencrypted length field that ends every pack.
const LENGTH_LEN: u64 = 4;

/// How much of a pack is read at a time while verifying it. Every byte of a pack
/// is read exactly once, and a bounded buffer means a large pack cannot turn the
/// check that protects it into an allocation. (rustic's own `check --read-data`
/// reads a whole pack into memory instead.)
const VERIFY_CHUNK: u64 = 8 * 1024 * 1024;

/// The AEAD overhead every ciphertext carries: a 16-byte nonce and a 16-byte
/// tag (`rustic_core` `src/crypto/aespoly1305.rs:78-96`), which is also what a
/// pack header's own length counts (`repofile/packfile.rs:33-35,368-372`).
const COMP_OVERHEAD: u64 = 32;

/// One blob as the pack's header describes it.
#[derive(Debug, Clone, Copy)]
struct Blob {
    /// The id the header names, i.e. the SHA-256 of the blob's plaintext.
    id: Id,
    /// Where the blob's ciphertext starts, counted from the pack's first byte.
    offset: u64,
    /// How long that ciphertext is, nonce and tag included.
    length: u64,
    /// The plaintext length, when the blob was stored compressed.
    uncompressed: Option<NonZeroU32>,
}

/// Read one pack's header and verify every blob in it, or say why it cannot be
/// trusted.
///
/// `hex` is the id the pack is stored under and `listed` the size the backend
/// gave for it; both come from the listing that found this pack, and the name is
/// checked against the bytes rather than assumed.
///
/// # Errors
///
/// A reason naming the failure — a pack whose bytes do not hash to its name, a
/// header that does not decrypt or does not parse, a length that disagrees with
/// the file, a blob that does not decrypt or does not hash to the id its header
/// gives it — as a `String`, because every caller only reports it.
pub fn verify_pack(
    be: &Arc<dyn WriteBackend>,
    hex: &str,
    listed: u64,
    mk: &MasterKey,
) -> Result<(), String> {
    let short = hex.get(..12).unwrap_or(hex);
    let id: Id = hex
        .parse()
        .map_err(|err| format!("pack {short} is not a valid id: {err}"))?;
    if listed < COMP_OVERHEAD + LENGTH_LEN {
        return Err(format!(
            "pack {short} is {listed} bytes, too short to hold a header"
        ));
    }

    // The last four bytes are the header's ciphertext length, unencrypted.
    let tail = read(be, &id, listed - LENGTH_LEN, LENGTH_LEN as u32)?;
    let header_len = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
    if header_len < COMP_OVERHEAD || header_len + LENGTH_LEN > listed {
        return Err(format!(
            "pack {short} declares a {header_len}-byte header, which does not fit a {listed}-byte pack"
        ));
    }

    let header = read(be, &id, listed - LENGTH_LEN - header_len, header_len as u32)?;
    let key = aead_key(mk);
    let plaintext = decrypt(&key, &header).map_err(|why| format!("pack {short}: {why}"))?;
    let (blobs, header_bytes) = parse_header(&plaintext)
        .map_err(|why| format!("pack {short} does not declare a usable header: {why}"))?;

    // What the header claims has to add up to the file on disk, in both
    // directions: the entries fill the header ciphertext (minus the AEAD
    // overhead), and the blobs plus that ciphertext fill the pack. rustic's own
    // header reader makes the same two checks (`repofile/packfile.rs:311-322`)
    // and `check_pack` after it (`commands/check.rs:727-757`); they are what
    // makes "every blob entry" a complete traversal of the file rather than a
    // walk over a prefix of it.
    if header_bytes + COMP_OVERHEAD != header_len {
        return Err(format!(
            "pack {short}'s header declares {header_bytes} bytes of entries, not the {} its length field gives",
            header_len - COMP_OVERHEAD
        ));
    }
    let blob_bytes: u64 = blobs.iter().map(|blob| blob.length).sum();
    if blob_bytes + header_len + LENGTH_LEN != listed {
        return Err(format!(
            "pack {short}'s header accounts for {} of its {listed} bytes",
            blob_bytes + header_len + LENGTH_LEN
        ));
    }

    // Read every blob the header names. The reads are sequential and go through
    // one bounded buffer rather than one call per blob: the blobs tile the
    // region in file order (the length check above is what says so), and on a
    // destination reached over the network a read is a round trip, so a 1 000
    // blob pack is 1 000 of them for no reason. The hasher is fed in file order
    // as the bytes arrive — the blobs, then the header, then the length field —
    // so the same pass that verifies the blobs also recomputes the id the pack
    // is stored under.
    let mut hasher = Sha256::new();
    let mut next = 0u64;
    let mut buffer: Vec<u8> = Vec::new();
    for (index, blob) in blobs.iter().enumerate() {
        while (buffer.len() as u64) < blob.length {
            // Only ever as far as this blob's own end: the header and the length
            // field are hashed after the region, not with it.
            let want = VERIFY_CHUNK.min(blob.length - buffer.len() as u64);
            let chunk = read(be, &id, next, want as u32)?;
            hasher.update(&chunk);
            next += chunk.len() as u64;
            buffer.extend_from_slice(&chunk);
        }
        // The buffered bytes cover `[next - buffer.len(), next)`, and the blob
        // is the first `length` of them: a header whose offsets are not the
        // accumulating ones this crate writes is a header this check refuses.
        let start = next - buffer.len() as u64;
        if blob.offset != start {
            return Err(format!(
                "pack {short}, blob {index}: its header puts it at {} of a region that reads at {start}",
                blob.offset
            ));
        }
        let ciphertext: Vec<u8> = buffer.drain(..blob.length as usize).collect();
        let bytes = decrypt(&key, &ciphertext)
            .map_err(|why| format!("pack {short}, blob {}: {why}", blob_hex(&blob.id)))?;
        let bytes = match blob.uncompressed {
            Some(length) => {
                let bytes = zstd::decode_all(&*bytes).map_err(|err| {
                    format!(
                        "pack {short}, blob {}: its bytes do not decompress: {err}",
                        blob_hex(&blob.id)
                    )
                })?;
                if bytes.len() != length.get() as usize {
                    return Err(format!(
                        "pack {short}, blob {}: decompresses to {} bytes, not the {length} its header gives",
                        blob_hex(&blob.id),
                        bytes.len()
                    ));
                }
                bytes
            }
            None => bytes,
        };
        // rustic's own comparison: the id is the SHA-256 of the plaintext, so
        // this is the same test its `check_pack` makes
        // (`commands/check.rs:802-805`) and its archiver relies on.
        if !blob
            .id
            .blob_matches_reader(bytes.len() as u64, &mut &bytes[..])
        {
            return Err(format!(
                "pack {short}, blob {index}: its plaintext is not the content of {}",
                blob_hex(&blob.id)
            ));
        }
    }
    hasher.update(&header);
    hasher.update(&tail);
    let computed = hex_digest(&hasher.finalize());
    if computed != *hex {
        return Err(format!(
            "pack {short} is not the bytes it is stored under: its contents hash to {}",
            computed.get(..12).unwrap_or(&computed)
        ));
    }

    Ok(())
}

/// A pack's own id, recomputed from the bytes that were read: SHA-256, which is
/// how rustic names a pack and what its own `check --read-data` recomputes
/// (`rustic_core` `src/blob/packer.rs:762`, `src/commands/check.rs:736`).
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A blob id as lowercase hex, for the refusal messages.
fn blob_hex(id: &Id) -> String {
    id.to_hex().as_str().to_string()
}

/// Read one exact range of a pack's stored bytes.
fn read(be: &Arc<dyn WriteBackend>, id: &Id, offset: u64, length: u32) -> Result<Vec<u8>, String> {
    let offset = u32::try_from(offset)
        .map_err(|_| format!("pack {id} is larger than a pack can be: offset {offset}"))?;
    // `false`: read the stored bytes, never a cached copy of them.
    let data = be
        .read_partial(FileType::Pack, id, false, offset, length)
        .map_err(|err| format!("pack {id} could not be read at {offset}+{length}: {err:#}"))?;
    if data.len() != length as usize {
        return Err(format!(
            "pack {id} ended after {} of the {length} bytes its header gives at {offset}",
            data.len()
        ));
    }
    Ok(data.to_vec())
}

/// Turn the decrypted header into the blobs it declares, in the order they lie
/// in the pack.
///
/// The parse is rustic's own: `HeaderEntry` is its type, with its `binrw`
/// reader, and the loop and the offset accumulation are the ones
/// `PackHeader::from_binary` makes (`repofile/packfile.rs:228-250`). The
/// returned byte count is the sum of the entry sizes, which the caller checks
/// against the header's own length field.
fn parse_header(plaintext: &[u8]) -> Result<(Vec<Blob>, u64), String> {
    const ENTRY_LEN: u64 = 37;
    const ENTRY_LEN_COMPRESSED: u64 = 41;

    let mut cursor = Cursor::new(plaintext);
    let mut blobs = Vec::new();
    let mut entries = 0u64;
    let mut offset = 0u64;
    loop {
        let entry = match HeaderEntry::read(&mut cursor) {
            Ok(entry) => entry,
            Err(err) if err.is_eof() => break,
            Err(err) => return Err(format!("it could not be read as pack entries: {err}")),
        };
        let (id, length, uncompressed, entry_len) = match entry {
            HeaderEntry::Data { len, id } => (id, len, None, ENTRY_LEN),
            HeaderEntry::Tree { len, id } => (id, len, None, ENTRY_LEN),
            HeaderEntry::CompData { len, len_data, id } => {
                (id, len, NonZeroU32::new(len_data), ENTRY_LEN_COMPRESSED)
            }
            HeaderEntry::CompTree { len, len_data, id } => {
                (id, len, NonZeroU32::new(len_data), ENTRY_LEN_COMPRESSED)
            }
        };
        let length = u64::from(length);
        offset = offset
            .checked_add(length)
            .ok_or_else(|| "its blob offsets overflow".to_string())?;
        entries += entry_len;
        blobs.push(Blob {
            id,
            offset: offset - length,
            length,
            uncompressed,
        });
    }
    if cursor.position() != plaintext.len() as u64 {
        return Err(format!(
            "{} of its header bytes are not entries",
            plaintext.len() as u64 - cursor.position()
        ));
    }
    Ok((blobs, entries))
}

/// The AEAD key `rustic_core` derives from a [`MasterKey`].
///
/// The layout is rustic's own: a 64-byte key made of the 32-byte AES-256 key it
/// keeps in `MasterKey::encrypt`, then poly1305's 16-byte `k` and 16-byte `r`
/// from `MasterKey::mac` — `MasterKey::key` calling `Key::from_keys(&self.encrypt,
/// &self.mac.k, &self.mac.r)` (`src/repofile/keyfile.rs:363-364`,
/// `src/crypto/aespoly1305.rs:22-60`). Both fields are public, which is why this
/// is reachable at all.
fn aead_key(mk: &MasterKey) -> AeadKey {
    let mut key = AeadKey::default();
    key[0..32].copy_from_slice(&mk.encrypt);
    key[32..48].copy_from_slice(&mk.mac.k);
    key[48..64].copy_from_slice(&mk.mac.r);
    key
}

/// Decrypt one AEAD ciphertext: `nonce ‖ ciphertext ‖ tag`, the exact layout
/// `CryptoKey::decrypt_data` implements (`src/crypto/aespoly1305.rs:78-96`).
///
/// These are the ten lines this module does not borrow. `rustic_core` 0.12.0
/// keeps them behind `pub(crate)`: `DecryptReadBackend` (and its
/// `read_encrypted_partial`), `CryptoKey` and `Key` all live in private modules
/// and are not re-exported — a probe against the pinned crate fails with
/// `module 'backend' is private` / `trait 'CryptoKey' is not publicly
/// re-exported` — while `MasterKey` is public and its key material is not. So
/// the alternative to these lines is not "rustic's own decryption" but no
/// decryption at all, and a pack that cannot be decrypted cannot be verified.
/// Everything around them — the key it is given, the entry parse, the pack
/// layout, the id comparison and the decompression — is rustic's own or the
/// crate the tool already depends on.
fn decrypt(key: &AeadKey, data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 16 {
        return Err(format!(
            "its ciphertext is {} bytes, shorter than an AEAD nonce",
            data.len()
        ));
    }
    let nonce = Nonce::from_slice(&data[0..16]);
    Aes256CtrPoly1305Aes::new(key)
        .decrypt(nonce, &data[16..])
        .map_err(|_| {
            "its bytes do not decrypt with this repository's key (MAC check failed)".to_string()
        })
}
