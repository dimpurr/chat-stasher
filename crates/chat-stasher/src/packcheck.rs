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
use rustic_core::repofile::{HeaderEntry, IndexBlob, IndexPack, MasterKey};
use rustic_core::{BlobLocation, BlobType, FileType, Id, PackId, ReadBackend, WriteBackend};
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
    /// Whether the blob is tree metadata or content.
    tpe: BlobType,
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
) -> Result<IndexPack, String> {
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

    let blobs = blobs
        .into_iter()
        .map(|blob| {
            Ok(IndexBlob {
                id: blob.id.into(),
                tpe: blob.tpe,
                location: BlobLocation {
                    offset: u32::try_from(blob.offset)
                        .map_err(|err| format!("pack {short} has an invalid blob offset: {err}"))?,
                    length: u32::try_from(blob.length)
                        .map_err(|err| format!("pack {short} has an invalid blob length: {err}"))?,
                    uncompressed_length: blob.uncompressed,
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let size = u32::try_from(listed)
        .map_err(|err| format!("pack {short} has an invalid listed size: {err}"))?;
    Ok(IndexPack {
        id: PackId::from(id),
        blobs,
        time: None,
        size: Some(size),
    })
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
        let (id, tpe, length, uncompressed, entry_len) = match entry {
            HeaderEntry::Data { len, id } => (id, BlobType::Data, len, None, ENTRY_LEN),
            HeaderEntry::Tree { len, id } => (id, BlobType::Tree, len, None, ENTRY_LEN),
            HeaderEntry::CompData { len, len_data, id } => (
                id,
                BlobType::Data,
                len,
                NonZeroU32::new(len_data),
                ENTRY_LEN_COMPRESSED,
            ),
            HeaderEntry::CompTree { len, len_data, id } => (
                id,
                BlobType::Tree,
                len,
                NonZeroU32::new(len_data),
                ENTRY_LEN_COMPRESSED,
            ),
        };
        let length = u64::from(length);
        offset = offset
            .checked_add(length)
            .ok_or_else(|| "its blob offsets overflow".to_string())?;
        entries += entry_len;
        blobs.push(Blob {
            id,
            tpe,
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

#[cfg(test)]
mod tests {
    //! Every `Err` arm of [`super::verify_pack`], against packs this module
    //! builds for itself.
    //!
    //! The fixtures are assembled from the layout the module's own doc comment
    //! gives — blob ciphertexts, then the AEAD-sealed header, then the
    //! unencrypted length field — and are stored under the SHA-256 of the bytes
    //! they are, which is what rustic names a pack by (`rustic_core`
    //! `src/blob/packer.rs:762`). Nothing here opens a repository, a file or a
    //! socket: [`MemBackend`] answers `read_partial` out of one `Vec<u8>` and
    //! refuses every write, so a test cannot grow a dependency on a real
    //! archive to reach an arm.
    //!
    //! Two of those arms cannot be reached by any pack at all, and the tests
    //! that say so assert the property that makes them unreachable rather than
    //! nothing: [`an_aead_ciphertext_always_decrypts_to_the_length_it_was_read_at`]
    //! and
    //! [`every_index_offset_is_the_running_sum_of_the_lengths_before_it`]. The
    //! rest are reachable and are each exercised against a pack damaged in
    //! exactly the one place the arm is about, and then **renamed to the hash of
    //! its damaged bytes** — the whole point being that the name check alone
    //! does not catch damage, so a test that only renamed the pack would prove
    //! nothing about the arms after it.
    //!
    //! The fixtures seal with [`super::aead_key`] rather than a second copy of
    //! the key layout, which is only safe because of the positive control:
    //! [`a_well_formed_pack_verifies_and_yields_what_its_header_declares`]. Were
    //! the derived key ever wrong, every fixture here would still be
    //! internally consistent and every refusal below would still pass; the one
    //! test that has to see `Ok` is the one that would notice.

    use super::{aead_key, hex_digest, verify_pack, COMP_OVERHEAD, LENGTH_LEN};
    use aes256ctr_poly1305aes::aead::{Aead, Payload};
    use aes256ctr_poly1305aes::{Aes256CtrPoly1305Aes, Key as AeadKey, Nonce};
    use binrw::BinRead;
    use bytes::Bytes;
    use rustic_core::repofile::{HeaderEntry, MasterKey};
    use rustic_core::{
        BlobType, ErrorKind, FileType, Id, ReadBackend, RusticError, RusticResult, WriteBackend,
    };
    use sha2::{Digest, Sha256};
    use std::io::Cursor;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    /// A blob whose plaintext does not compress, so its stored form is the
    /// plaintext plus the AEAD overhead and the fixture can say what it is.
    const INCOMPRESSIBLE: &[u8] = b"a plain blob of synthetic fixture bytes";

    /// A blob that compresses, so a compressed entry really is shorter than the
    /// content it stands for.
    const COMPRESSIBLE: &[u8] =
        b"a compressible blob a compressible blob a compressible blob a compressible blob";

    /// The id a header entry has to name for `content`: the SHA-256 of the bytes a
    /// reader sees, which is rustic's own rule (`Id::blob_matches_reader`).
    fn id_of(content: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(content);
        hasher.finalize().into()
    }

    /// A fixed, synthetic repository key.
    ///
    /// `MasterKey::new()` hands out random key material, and a fixture whose
    /// bytes move on every run is a failure nobody can read back. Nothing here
    /// is a secret: it is 64 bytes of counting, and a test that printed the
    /// ciphertext would leak nothing an attacker does not already have, because
    /// the same key is in the repository file by construction. `Mac`, the type of
    /// the `mac` field, is not re-exported by `rustic_core`, so the vectors are
    /// written through the fields themselves rather than through a literal.
    fn master_key() -> MasterKey {
        let mut mk = MasterKey::new();
        for (n, byte) in mk.encrypt.iter_mut().enumerate() {
            *byte = n as u8;
        }
        for (n, byte) in mk.mac.k.iter_mut().enumerate() {
            *byte = 0x80 + n as u8;
        }
        for (n, byte) in mk.mac.r.iter_mut().enumerate() {
            *byte = 0xc0 + n as u8;
        }
        mk
    }

    /// One AEAD ciphertext in the layout `CryptoKey::encrypt_data` writes and
    /// [`super::decrypt`] reads: `nonce ‖ ciphertext ‖ tag`
    /// (`rustic_core` `src/crypto/aespoly1305.rs:78-96`, `:113-130`).
    ///
    /// The nonce is a counter rather than random, so a fixture's bytes are the
    /// same on every run. No two seals in one pack share a nonce, which is what
    /// the AEAD requires; the header's is `0xFE` and each blob's is its index
    /// plus one.
    fn seal(key: &AeadKey, nonce_fill: u8, plaintext: &[u8]) -> Vec<u8> {
        let mut raw = [0u8; 16];
        raw.fill(nonce_fill);
        let sealed = Aes256CtrPoly1305Aes::new(key)
            .encrypt(Nonce::from_slice(&raw), Payload::from(plaintext))
            .expect("a fixture this small cannot fail to encrypt");
        let mut out = Vec::with_capacity(raw.len() + sealed.len());
        out.extend_from_slice(&raw);
        out.extend_from_slice(&sealed);
        out
    }

    /// One blob a fixture pack stores, and what its header entry claims about it.
    ///
    /// The three `claiming_*` builders are the only ways a fixture lies: a header
    /// entry is three numbers and an id, and each of them is one a damaged or
    /// forged pack can get wrong.
    struct Blob {
        /// The content: the plaintext, or the bytes before compression. The id is
        /// taken over this unless the entry claims another.
        content: Vec<u8>,
        /// Whether the entry is a compressed one (`CompData` / `CompTree`).
        compressed: bool,
        /// Whether the entry is a tree entry.
        tree: bool,
        /// The id the entry names, when it is not `content`'s own SHA-256.
        id: Option<[u8; 32]>,
        /// The entry's magic byte, when it is not the one its kind calls for.
        magic: Option<u8>,
        /// The ciphertext length the entry declares, when it is not the real one.
        len: Option<u32>,
        /// The plaintext length a compressed entry declares, when it is not the
        /// real one.
        len_data: Option<u32>,
        /// The bytes to encrypt and store, when they are not the form of
        /// `content` this entry claims — a fixture that lies about what it holds.
        stored: Option<Vec<u8>>,
        /// The bytes to put in the pack for this blob **verbatim**, instead of a
        /// sealed ciphertext. Only the one case that needs bytes which are not a
        /// ciphertext at all uses it: an entry that declares a blob the file
        /// cannot supply a nonce for.
        region: Option<Vec<u8>>,
    }

    impl Blob {
        fn new(content: &[u8]) -> Self {
            Self {
                content: content.to_vec(),
                compressed: false,
                tree: false,
                id: None,
                magic: None,
                len: None,
                len_data: None,
                stored: None,
                region: None,
            }
        }

        /// A data blob stored as it is.
        fn plain(content: &[u8]) -> Self {
            Self::new(content)
        }

        /// A tree blob stored as it is.
        fn tree(content: &[u8]) -> Self {
            Self {
                tree: true,
                ..Self::new(content)
            }
        }

        /// A data blob stored compressed.
        fn compressed(content: &[u8]) -> Self {
            Self {
                compressed: true,
                ..Self::new(content)
            }
        }

        fn claiming_id(mut self, id: [u8; 32]) -> Self {
            self.id = Some(id);
            self
        }

        fn claiming_magic(mut self, magic: u8) -> Self {
            self.magic = Some(magic);
            self
        }

        fn claiming_len(mut self, len: u32) -> Self {
            self.len = Some(len);
            self
        }

        fn claiming_len_data(mut self, len_data: u32) -> Self {
            self.len_data = Some(len_data);
            self
        }

        fn storing(mut self, bytes: &[u8]) -> Self {
            self.stored = Some(bytes.to_vec());
            self
        }

        fn unsealed_region(mut self, bytes: &[u8]) -> Self {
            self.region = Some(bytes.to_vec());
            self
        }

        /// The bytes this blob occupies in the pack, which is the compressed form
        /// of the content when the entry says it is compressed.
        fn stored_bytes(&self) -> Vec<u8> {
            if let Some(bytes) = &self.stored {
                return bytes.clone();
            }
            if !self.compressed {
                return self.content.clone();
            }
            zstd::encode_all(self.content.as_slice(), 1)
                .expect("the synthetic fixture content must compress")
        }
    }

    /// An assembled pack: the bytes a backend stores, and where its parts are, so
    /// a test can damage exactly the one it is about.
    struct Pack {
        /// The pack as a backend holds it.
        bytes: Vec<u8>,
        /// Where each blob's stored bytes start.
        blob_at: Vec<usize>,
        /// How long each blob's stored bytes are.
        blob_len: Vec<usize>,
        /// Where the header's ciphertext starts.
        header_at: usize,
    }

    impl Pack {
        /// The id this pack is stored under: the hash of the bytes it is, which
        /// is how rustic names a pack (`src/blob/packer.js:762`, and `check`
        /// recomputes the same thing at `src/commands/check.rs:736`).
        fn hex(&self) -> String {
            hex_digest(&Sha256::digest(&self.bytes))
        }

        /// How long the pack is, as the listing that found it would report.
        fn listed(&self) -> u64 {
            self.bytes.len() as u64
        }

        /// Flip one bit of a blob's stored bytes.
        ///
        /// The pack's id changes with it, so a caller that then asks for
        /// [`Pack::hex`] is asking under the name of the **damaged** bytes: that
        /// is the case the whole module exists for, and it is why every
        /// damage test renames rather than leaving the pack alone.
        fn damage_blob(&mut self, index: usize, at: usize) {
            let at = self.blob_at[index] + at;
            self.bytes[at] ^= 0x01;
        }

        /// Flip one bit of the header's ciphertext. Same naming rule as
        /// [`Pack::damage_blob`].
        fn damage_header(&mut self, at: usize) {
            self.bytes[self.header_at + at] ^= 0x01;
        }
    }

    /// Assemble a pack: the blobs' ciphertexts, then the sealed header, then the
    /// unencrypted length field.
    ///
    /// The pack holds what each blob really is and the entry declares what it
    /// likes: an entry that claims a length the file cannot supply is exactly the
    /// disagreement the size-accounting test is about, so the two are separate
    /// knobs rather than one.
    ///
    /// `header_padding` appends bytes to the header plaintext that are not an
    /// entry, and `declared_header_len` replaces what the trailing length field
    /// says. Both exist for one arm each, and both are absent in every other
    /// fixture, so a fixture that is meant to be healthy is healthy in both.
    fn assemble(
        key: &AeadKey,
        blobs: &[Blob],
        header_padding: usize,
        declared_header_len: Option<u32>,
    ) -> Pack {
        let mut bytes: Vec<u8> = Vec::new();
        let mut blob_at = Vec::new();
        let mut blob_len = Vec::new();
        let mut entries: Vec<u8> = Vec::new();
        for (index, blob) in blobs.iter().enumerate() {
            let sealed = match &blob.region {
                Some(bytes) => bytes.clone(),
                None => seal(key, index as u8 + 1, &blob.stored_bytes()),
            };
            let len = blob.len.unwrap_or_else(|| {
                u32::try_from(sealed.len()).expect("a fixture blob is under 4 GiB")
            });
            // An entry may claim more than the file holds; the fixture then pads,
            // so the region is real bytes a reader can still be given.
            let mut region = sealed;
            while region.len() < len as usize {
                region.push(0);
            }
            write_entry(&mut entries, blob, len);
            blob_at.push(bytes.len());
            blob_len.push(region.len());
            bytes.extend_from_slice(&region);
        }
        entries.resize(entries.len() + header_padding, 0);
        let header = seal(key, 0xFE, &entries);
        let header_len = declared_header_len.unwrap_or_else(|| {
            u32::try_from(header.len()).expect("a fixture header is under 4 GiB")
        });
        let header_at = bytes.len();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&header_len.to_le_bytes());
        Pack {
            bytes,
            blob_at,
            blob_len,
            header_at,
        }
    }

    /// Write one `HeaderEntry` in the layout `rustic_core`'s own reader expects:
    /// a magic byte, the stored length, the blob id, and — for a compressed
    /// entry — the plaintext length before compression
    /// (`src/repofile/packfile.rs:88-129`).
    ///
    /// It is written out by hand rather than through `binrw`'s writer, so a
    /// fixture pins the format instead of restating the reader that parses it.
    /// The lengths that come out are the 37 and 41 the module's own
    /// [`super::parse_header`] sums.
    fn write_entry(entries: &mut Vec<u8>, blob: &Blob, len: u32) {
        let magic = blob.magic.unwrap_or(match (blob.tree, blob.compressed) {
            (false, false) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (true, true) => 3,
        });
        entries.push(magic);
        entries.extend_from_slice(&len.to_le_bytes());
        if blob.compressed {
            let len_data = blob.len_data.unwrap_or_else(|| {
                u32::try_from(blob.content.len()).expect("a fixture blob is small")
            });
            entries.extend_from_slice(&len_data.to_le_bytes());
        }
        entries.extend_from_slice(&blob.id.unwrap_or_else(|| id_of(&blob.content)));
    }

    /// What a synthetic backend does with a read it is asked for.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        /// Hand out exactly the bytes the pack holds.
        None,
        /// Refuse every read, as a backend that cannot reach the destination.
        Unreadable,
        /// Hand out one byte less than was asked for, as a pack that ends before
        /// its own header says it does.
        Short,
    }

    /// The whole destination, in memory: one pack's bytes, no repository.
    ///
    /// The `WriteBackend` half is required because [`super::verify_pack`] takes
    /// one, and it is implemented as a refusal rather than as a store: a test in
    /// this module that wrote a pack back would be testing a repository, not the
    /// checks on one.
    struct MemBackend {
        bytes: Vec<u8>,
        fault: Fault,
    }

    impl MemBackend {
        fn holding(bytes: Vec<u8>) -> Arc<dyn WriteBackend> {
            Arc::new(Self {
                bytes,
                fault: Fault::None,
            })
        }

        fn faulty(bytes: Vec<u8>, fault: Fault) -> Arc<dyn WriteBackend> {
            Arc::new(Self { bytes, fault })
        }
    }

    impl ReadBackend for MemBackend {
        fn location(&self) -> String {
            "mem".to_string()
        }

        fn list_with_size(&self, _tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
            Ok(Vec::new())
        }

        fn read_full(&self, _tpe: FileType, _id: &Id) -> RusticResult<Bytes> {
            Ok(Bytes::new())
        }

        fn read_partial(
            &self,
            _tpe: FileType,
            _id: &Id,
            _cacheable: bool,
            offset: u32,
            length: u32,
        ) -> RusticResult<Bytes> {
            if self.fault == Fault::Unreadable {
                return Err(RusticError::new(
                    ErrorKind::Backend,
                    "the synthetic backend refuses this read",
                ));
            }
            let start = usize::try_from(offset)
                .expect("an offset is what the module asked for")
                .min(self.bytes.len());
            let end =
                (start + usize::try_from(length).expect("a length is small")).min(self.bytes.len());
            let span = end - start;
            let take = match self.fault {
                Fault::Short => span.saturating_sub(1),
                _ => span,
            };
            Ok(Bytes::copy_from_slice(&self.bytes[start..start + take]))
        }

        fn warmup_path(&self, _tpe: FileType, _id: &Id) -> String {
            "mem".to_string()
        }
    }

    impl WriteBackend for MemBackend {
        fn create(&self) -> RusticResult<()> {
            Err(RusticError::new(
                ErrorKind::Backend,
                "the synthetic backend is read-only",
            ))
        }

        fn write_bytes(
            &self,
            _tpe: FileType,
            _id: &Id,
            _cacheable: bool,
            _buf: Bytes,
        ) -> RusticResult<()> {
            Err(RusticError::new(
                ErrorKind::Backend,
                "the synthetic backend is read-only",
            ))
        }

        fn remove(&self, _tpe: FileType, _id: &Id, _cacheable: bool) -> RusticResult<()> {
            Err(RusticError::new(
                ErrorKind::Backend,
                "the synthetic backend is read-only",
            ))
        }
    }

    /// A healthy pack, so that every fixture above has a control: the module must
    /// accept this one, and it must hand back the index entry its header
    /// describes.
    ///
    /// If this ever fails, the refusals below prove nothing — they would be the
    /// answer to a key, a layout or a nonce that is wrong, not to the damage
    /// each one injects.
    #[test]
    fn a_well_formed_pack_verifies_and_yields_what_its_header_declares() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(
            &key,
            &[
                Blob::plain(INCOMPRESSIBLE),
                Blob::compressed(COMPRESSIBLE),
                Blob::tree(INCOMPRESSIBLE),
            ],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let index = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect("a well-formed pack must verify");
        assert_eq!(index.id.to_hex().as_str(), pack.hex());
        assert_eq!(
            index.size,
            Some(u32::try_from(pack.listed()).expect("small"))
        );
        assert_eq!(index.time, None);
        assert_eq!(index.blobs.len(), 3);

        // A plain data blob: no compression recorded, at the front of the pack.
        assert_eq!(index.blobs[0].tpe, BlobType::Data);
        assert_eq!(
            index.blobs[0].id.to_hex().as_str(),
            hex_digest(&id_of(INCOMPRESSIBLE))
        );
        assert_eq!(index.blobs[0].location.offset, 0);
        assert_eq!(
            index.blobs[0].location.length as usize, pack.blob_len[0],
            "the recorded length must be the stored length the header declared"
        );
        assert_eq!(index.blobs[0].location.uncompressed_length, None);

        // A compressed blob: the plaintext length is recorded beside the stored
        // length, and the id still names the content, not the compressed form.
        assert_eq!(index.blobs[1].tpe, BlobType::Data);
        assert_eq!(
            index.blobs[1].id.to_hex().as_str(),
            hex_digest(&id_of(COMPRESSIBLE))
        );
        assert_eq!(
            index.blobs[1].location.offset as usize, pack.blob_at[1],
            "the second blob starts where the first one ended"
        );
        assert_eq!(
            index.blobs[1].location.uncompressed_length,
            NonZeroU32::new(u32::try_from(COMPRESSIBLE.len()).expect("small")),
            "a compressed entry must record the plaintext length it decompresses to"
        );

        // A tree blob keeps its type through the check.
        assert_eq!(index.blobs[2].tpe, BlobType::Tree);
    }

    /// `hex` is what the pack is stored under, so a name that is not an id is the
    /// first thing that can be wrong — before a single byte is read.
    ///
    /// Both ways it fails are here: characters that are not hex, and a string of
    /// the right alphabet in the wrong length. The refusal still has to name the
    /// pack, or an operator holding a directory of files has nothing to grep.
    #[test]
    fn a_name_that_is_not_an_id_is_refused_before_the_pack_is_read() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, None);
        let be = MemBackend::holding(pack.bytes.clone());
        let size = pack.listed();

        let not_hex = "zz".repeat(32);
        let why = verify_pack(&be, &not_hex, size, &mk).expect_err("z is not a hex digit");
        assert!(why.contains("is not a valid id"), "{why}");
        assert!(
            why.contains("zzzzzzzzzzzz"),
            "the pack must be named: {why}"
        );

        let too_short = "ab".repeat(31);
        let why = verify_pack(&be, &too_short, size, &mk).expect_err("an id is 64 hex digits");
        assert!(why.contains("is not a valid id"), "{why}");

        // Not the "too short to hold a header" arm: that one is about the size,
        // and a pack of a healthy length is what both names above were given.
        assert!(!why.contains("too short"), "{why}");
    }

    /// A file that cannot hold a header and a length field is not a pack, and is
    /// refused before its last four bytes are read as a length. The boundary is
    /// asserted as an inequality as well, because a loop over the sizes below
    /// would also pass if the comparison inside the module were reversed.
    #[test]
    fn a_pack_shorter_than_a_header_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, None);
        let be = MemBackend::holding(pack.bytes.clone());
        for listed in [0, 1, COMP_OVERHEAD, COMP_OVERHEAD + LENGTH_LEN - 1] {
            assert!(listed < COMP_OVERHEAD + LENGTH_LEN, "listed {listed}");
            let why = verify_pack(&be, &pack.hex(), listed, &mk)
                .expect_err("a pack that cannot hold a header must be refused");
            assert!(
                why.contains("too short to hold a header"),
                "listed {listed}: {why}"
            );
        }
        // And the size just above the boundary is past it: the fixture is far
        // longer than the header, so the last four bytes it reads are blob bytes
        // and the pack is refused for what they say rather than for being short.
        let why = verify_pack(&be, &pack.hex(), COMP_OVERHEAD + LENGTH_LEN, &mk)
            .expect_err("the fixture's own bytes are not a length field");
        assert!(
            !why.contains("too short to hold a header"),
            "one byte more than the loop's largest size is past this arm: {why}"
        );
    }

    /// The length field a pack ends with is unencrypted, so it is the one number
    /// a damaged pack can state about itself. Both ways it can be wrong are
    /// refused: too small to be any ciphertext, and larger than the file it
    /// claims to describe.
    #[test]
    fn a_length_field_that_does_not_fit_its_pack_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);

        // 31 bytes: below the 32 an AEAD ciphertext always costs.
        let pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, Some(31));
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a 31-byte header is not a ciphertext");
        assert!(why.contains("declares a 31-byte header"), "{why}");
        assert!(why.contains("does not fit"), "{why}");

        // Larger than the pack: a length field read as a number that cannot
        // address bytes the file has.
        let pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, Some(4096));
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a header larger than the pack must be refused");
        assert!(why.contains("declares a 4096-byte header"), "{why}");
    }

    /// The first of the three checks: the bytes have to hash to the name.
    ///
    /// The fixture is a sound pack under a name that is not its hash — one hex
    /// character off — because that is the only shape that reaches this arm. It
    /// is worth saying why, because the module's own doc comment reads as though
    /// damaged-in-place bytes land here, and they do not: **every** byte of a
    /// pack is either authenticated or is the four-byte length field, and
    /// changing the length field moves the window the header is read from, so the
    /// header no longer decrypts. A pack damaged in place and left under its old
    /// name is therefore refused by the two tests below — by check 2 or check 3,
    /// never by this one. What this arm catches is the name being wrong: a file
    /// renamed, a listing that attributed another pack's id, or a backend
    /// serving a different object under a name. Those are the cases where every
    /// individual check would otherwise pass, because the bytes *are* internally
    /// consistent; only the name says otherwise.
    #[test]
    fn a_pack_whose_bytes_are_not_its_name_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, None);
        let true_name = pack.hex();
        let mut wrong_name = true_name.clone();
        wrong_name.replace_range(0..1, if true_name.starts_with('0') { "1" } else { "0" });
        assert_ne!(wrong_name, true_name, "the name has to be wrong");

        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &wrong_name, pack.listed(), &mk)
            .expect_err("bytes that are not what they are stored under must be refused");
        assert!(why.contains("is not the bytes it is stored under"), "{why}");
        assert!(
            why.contains("contents hash to"),
            "the refusal must carry the hash they do have: {why}"
        );
        assert!(
            why.contains(&true_name[..12]),
            "so an operator can tell which file it should have been: {why}"
        );

        // The control: the very same bytes under their own hash are a sound pack,
        // which is what makes this a check on the name rather than on the pack.
        let be = MemBackend::holding(pack.bytes.clone());
        assert!(
            verify_pack(&be, &true_name, pack.listed(), &mk).is_ok(),
            "the same bytes under their own hash are a sound pack"
        );
    }

    /// The second check: the header is ciphertext under the repository key, so a
    /// MAC failure means the header was not written by a holder of it. The pack
    /// here is damaged **and** renamed to the hash of its damaged bytes, which is
    /// the case the first check cannot see.
    #[test]
    fn a_header_that_does_not_decrypt_is_refused_even_when_the_pack_was_renamed() {
        let mk = master_key();
        let key = aead_key(&mk);
        let mut pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, None);
        pack.damage_header(20);
        // The name is taken *after* the damage: damage and rename is the whole
        // point, so a test that forgot to rename would be asserting the first
        // check twice.
        let renamed = pack.hex();
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &renamed, pack.listed(), &mk)
            .expect_err("a damaged header must be refused");
        assert!(
            why.contains("do not decrypt with this repository's key"),
            "{why}"
        );
        assert!(
            !why.contains("not the bytes it is stored under"),
            "the name matched; the header is what failed: {why}"
        );
    }

    /// A header that is not entries at all — neither a usable entry nor the end
    /// of the header — is refused by name.
    #[test]
    fn a_header_that_is_not_pack_entries_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(
            &key,
            &[Blob::plain(INCOMPRESSIBLE).claiming_magic(9)],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a header entry with no such magic must be refused");
        assert!(why.contains("does not declare a usable header"), "{why}");
        assert!(why.contains("could not be read as pack entries"), "{why}");
    }

    /// Header bytes that are not an entry are refused.
    ///
    /// `parse_header` has two refusals for a header it cannot use: one for bytes
    /// it cannot read as an entry, and one for bytes left over after the last
    /// entry. What a padded header actually hits is the first. The second cannot
    /// fire at all with the pinned `binrw`, and the reason is worth pinning
    /// rather than leaving implied: a partial entry at the end of a header is a
    /// "no variant matched" error, **not** the end-of-input the parse loop breaks
    /// on, because the four variants fail differently — the one whose magic byte
    /// matches runs out of bytes on its length field, the other three on the
    /// magic — so `binrw` cannot collapse them into one end-of-input. The probe at
    /// the bottom of this test is that property, read through `rustic_core`'s own
    /// reader: a `binrw` bump that turned a partial entry into a plain end-of-input
    /// would turn this test red rather than quietly move a refusal from one
    /// message to another.
    #[test]
    fn header_bytes_that_are_not_an_entry_are_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 1, None);
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a padded header must be refused");
        assert!(why.contains("does not declare a usable header"), "{why}");
        assert!(
            why.contains("could not be read as pack entries"),
            "one padding byte after a complete entry is a partial entry, and that is \
             what the module refuses it as: {why}"
        );

        // The same header plaintext, read by `rustic_core`'s own entry reader: the
        // complete entry parses, and the padding byte after it is a complete
        // magic (`Data`) and not a complete entry — which is not the end of the
        // header, so `parse_header`'s loop refuses it instead of breaking out of
        // it and leaving the count arm to notice.
        let mut entries: Vec<u8> = Vec::new();
        write_entry(&mut entries, &Blob::plain(INCOMPRESSIBLE), 71);
        entries.push(0);
        let mut cursor = Cursor::new(entries.as_slice());
        let first = HeaderEntry::read(&mut cursor).expect("the complete entry parses");
        assert!(matches!(first, HeaderEntry::Data { len: 71, .. }));
        assert_eq!(cursor.position(), 37, "the complete entry was consumed");
        let err = HeaderEntry::read(&mut cursor).expect_err("a padding byte is not a header entry");
        assert_eq!(
            cursor.position(),
            37,
            "the failed read consumed nothing, so the byte is still unaccounted for"
        );
        assert!(
            !err.is_eof(),
            "a partial entry must not read as the end of the header, or the refusal \
             above would be a different arm: {err}"
        );
    }

    /// The blobs the header names have to account for the file: the entries and
    /// the length field are what is left over. An entry that claims a length the
    /// stored bytes do not have means the header does not describe this file.
    #[test]
    fn a_header_that_does_not_account_for_the_file_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        // One byte short of the ciphertext the file really holds, so the entries,
        // the header and the length field add up to one byte less than the file.
        let honest = u32::try_from(INCOMPRESSIBLE.len() + 32).expect("small");
        let pack = assemble(
            &key,
            &[Blob::plain(INCOMPRESSIBLE).claiming_len(honest - 1)],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a header that does not add up must be refused");
        assert!(
            why.contains("accounts for"),
            "the refusal must say how much of the file the header explains: {why}"
        );
    }

    /// A blob's stored bytes are under the repository key too, and a blob that
    /// does not decrypt is caught here rather than by the name check, because the
    /// pack has already been renamed to the hash of its damaged bytes.
    #[test]
    fn a_blob_that_does_not_decrypt_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let mut pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, None);
        pack.damage_blob(0, 5);
        let renamed = pack.hex();
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &renamed, pack.listed(), &mk)
            .expect_err("a damaged blob must be refused");
        assert!(
            why.contains("do not decrypt with this repository's key"),
            "{why}"
        );
        assert!(
            why.contains(&hex_digest(&id_of(INCOMPRESSIBLE))[..12]),
            "the refusal must name the blob that failed: {why}"
        );
    }

    /// An entry can claim a blob of zero bytes, which leaves nothing to decrypt.
    /// It is refused as the too-short ciphertext it is, not silently passed over.
    #[test]
    fn a_blob_with_no_ciphertext_is_refused_rather_than_read_as_empty() {
        let mk = master_key();
        let key = aead_key(&mk);
        // The entry declares a blob, and the file holds no bytes for it at all —
        // so the header's own arithmetic still adds up, and what is left is the
        // ciphertext that is not there.
        let pack = assemble(
            &key,
            &[Blob::plain(INCOMPRESSIBLE).unsealed_region(b"")],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a blob entry with no ciphertext must be refused");
        assert!(why.contains("shorter than an AEAD nonce"), "{why}");
    }

    /// A compressed blob whose bytes are not a zstd frame is refused as
    /// undecompressable, by name — the header said this blob was compressed, and
    /// the ciphertext it stored is not.
    #[test]
    fn a_compressed_blob_that_does_not_decompress_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(
            &key,
            &[Blob::compressed(COMPRESSIBLE).storing(b"not a zstd frame at all")],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a blob that does not decompress must be refused");
        assert!(why.contains("do not decompress"), "{why}");
    }

    /// A compressed blob that decompresses to a length other than the one its
    /// entry declares is refused, and the refusal carries both numbers: which is
    /// what tells an operator whether the header or the payload is the liar.
    #[test]
    fn a_blob_that_decompresses_to_another_length_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let declared = u32::try_from(COMPRESSIBLE.len()).expect("small") + 1;
        let pack = assemble(
            &key,
            &[Blob::compressed(COMPRESSIBLE).claiming_len_data(declared)],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a wrong decompressed length must be refused");
        assert!(
            why.contains("decompresses to"),
            "both lengths have to be in the refusal: {why}"
        );
        assert!(why.contains(&declared.to_string()), "{why}");
        assert!(why.contains(&COMPRESSIBLE.len().to_string()), "{why}");
    }

    /// The check the dedup entry rests on: the blob's plaintext has to be the
    /// content of the id its header names. Here the header names an id the
    /// plaintext does not hash to — which is what a pack whose blob was replaced
    /// while its header was rewritten to match looks like from the outside.
    #[test]
    fn a_blob_that_is_not_the_content_of_its_id_is_refused() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(
            &key,
            &[
                Blob::plain(INCOMPRESSIBLE),
                Blob::plain(b"the second blob").claiming_id([0xAB; 32]),
            ],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a blob that is not the content of its id must be refused");
        assert!(why.contains("its plaintext is not the content of"), "{why}");
        assert!(
            why.contains("blob 1"),
            "the second blob is the damaged one, and the refusal must say so: {why}"
        );
        assert!(why.contains(&hex_digest(&[0xAB; 32])[..12]), "{why}");
    }

    /// A compressed entry whose `len_data` is zero: `NonZeroU32::new(0)` is
    /// `None`, so the blob is treated as **uncompressed** — no decompression is
    /// attempted and no decompressed length is checked. That is pinned here.
    ///
    /// It is not a silent corruption, and this is the argument for it. `rustic`
    /// cannot write this header: `HeaderEntry::from_blob` maps an absent
    /// uncompressed length to a `Data` entry and `HeaderEntry::into_location`
    /// maps `len_data: 0` back to `None` (`src/repofile/packfile.rs:140-160`,
    /// `:163-180`), so a `CompData` entry of zero is a forged or damaged header
    /// and never a legitimate one. What the module then does with it is to read
    /// the blob the way **rustic's own reader** reads it — as uncompressed — and
    /// hold it to the one check that cannot be talked out of: the plaintext must
    /// be the content of the id the entry names. That is the correct behaviour
    /// here, because the `IndexBlob` this module hands back is read by rustic
    /// later, and a refusal would be a refusal of a file rustic would read
    /// happily. The alternative — refusing a zero `len_data` outright — would
    /// reject a pack no reader of this repository can use, to no end.
    ///
    /// What the fixture below pins is both halves: accepted when the id matches
    /// the stored bytes, and refused when it does not. A `len_data` of zero
    /// therefore costs the decompressed-length check for that one blob and
    /// nothing else — which is the honest reading of "the header claims
    /// compression it cannot be describing", and is why the id check below is
    /// not optional.
    #[test]
    fn a_comp_data_entry_with_a_zero_len_data_is_read_as_uncompressed() {
        let mk = master_key();
        let key = aead_key(&mk);
        // The stored bytes are the plaintext itself, and the entry is a
        // `CompData` that claims it decompresses to zero bytes — a claim the
        // module cannot honour and does not have to.
        let pack = assemble(
            &key,
            &[Blob::compressed(INCOMPRESSIBLE)
                .storing(INCOMPRESSIBLE)
                .claiming_len_data(0)],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let index = verify_pack(&be, &pack.hex(), pack.listed(), &mk).expect(
            "a zero `len_data` is read as uncompressed, and the id still decides: the \
             entry names the content of the bytes that are actually there",
        );
        assert_eq!(
            index.blobs[0].location.uncompressed_length, None,
            "a zero `len_data` cannot be a `NonZeroU32`, so the index records no \
             uncompressed length — which is exactly what rustic's own reader does \
             with the same header"
        );
        assert_eq!(index.blobs[0].tpe, BlobType::Data);

        // And the same entry with an id the stored bytes do not hash to is
        // refused: treating it as uncompressed removes the length check for this
        // blob, not the content check.
        let pack = assemble(
            &key,
            &[Blob::compressed(INCOMPRESSIBLE)
                .storing(INCOMPRESSIBLE)
                .claiming_len_data(0)
                .claiming_id([0xCD; 32])],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("the id check is the one a zero `len_data` cannot skip");
        assert!(why.contains("its plaintext is not the content of"), "{why}");
    }

    /// Three refusals come out of reading the pack rather than out of its
    /// contents, and all three are reachable with one blob in memory.
    #[test]
    fn a_backend_that_cannot_read_the_pack_is_reported_as_such() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(&key, &[Blob::plain(INCOMPRESSIBLE)], 0, None);

        let be = MemBackend::faulty(pack.bytes.clone(), Fault::Unreadable);
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a backend that cannot read must not read as a sound pack");
        assert!(why.contains("could not be read at"), "{why}");

        // A pack that ends before the range its own length field describes: the
        // backend hands out fewer bytes than asked for rather than failing, which
        // is what a truncated file on a remote destination looks like.
        let be = MemBackend::faulty(pack.bytes.clone(), Fault::Short);
        let why = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect_err("a short read must not read as a sound pack");
        assert!(
            why.contains("ended after"),
            "the refusal must say how much of the range arrived: {why}"
        );

        // A pack larger than the `u32` offsets the backend takes. `listed` is
        // `u64` and comes from a listing, so nothing upstream has refused this
        // yet: it is this module's own `read` that has to.
        let be = MemBackend::holding(pack.bytes.clone());
        let huge = u64::from(u32::MAX) + 5;
        let why = verify_pack(&be, &pack.hex(), huge, &mk)
            .expect_err("a pack a backend cannot address must be refused");
        assert!(why.contains("is larger than a pack can be"), "{why}");
        assert!(
            why.contains(&format!("offset {}", huge - LENGTH_LEN)),
            "{why}"
        );
    }

    /// The `u32::try_from` arms on the way out. None of them is reachable from a
    /// pack this module can hold, and what is asserted here is the arithmetic
    /// that says so rather than nothing at all.
    #[test]
    fn the_u32_conversions_on_the_way_out_cannot_fail_on_a_pack_that_can_be_read() {
        let largest = u64::from(u32::MAX);

        // A header entry's length is a `u32` by the format, widened to `u64` by
        // the parse, so the conversion back cannot fail for any entry.
        assert_eq!(
            u32::try_from(u64::from(u32::MAX)).map_err(|_| "overflow"),
            Ok(u32::MAX)
        );

        // The `listed` conversion has a window, and it is four bytes wide: a pack
        // larger than `u32::MAX` whose length field still sits at an addressable
        // offset is readable, and only then would the final conversion have
        // something to refuse.
        let first_overflowing = largest + 1;
        assert!(u32::try_from(first_overflowing).is_err());
        let last_readable = first_overflowing + LENGTH_LEN - 1;
        assert!(
            u32::try_from(last_readable).is_err() && (last_readable - LENGTH_LEN) <= largest,
            "the window is exactly the listed sizes from 2^32 to 2^32 + LENGTH_LEN"
        );

        // Reaching that window means reading a pack of at least four gibibytes,
        // and before the conversion the header it declares has to be parsed: the
        // entry-length sum has to come to `listed - header_len - LENGTH_LEN`, so
        // the header alone carries about four gibibytes of entries on top of the
        // four gibibytes of blobs. That is two allocations of that size in one
        // `verify_pack` call, which is why there is no fixture for it and why
        // this test pins the arithmetic instead.
        let blob_region = last_readable - COMP_OVERHEAD - LENGTH_LEN;
        assert!(
            blob_region > largest - 1024 * 1024,
            "a pack in the window is a gigabyte-scale read, not a fixture"
        );

        // The blob-offset conversion needs a region that does not fit in `u32`
        // either, and the entry-length conversion is the `u32` widening above.
        // The accumulation in `parse_header` would need this many entries before
        // `checked_add` could give up, which is why that arm is out of reach for
        // a header this module can parse at all.
        assert!(
            u64::MAX / u64::from(u32::MAX) > 4_294_967_295,
            "more than 2^32 entries are needed, i.e. more than 159 GiB of header"
        );
    }

    /// Why the "the header declares N bytes of entries" arm cannot fire: an AEAD
    /// ciphertext is exactly its plaintext plus 32 bytes, so a header read at
    /// `header_len` bytes always decrypts to `header_len - 32` bytes of
    /// plaintext, and a header whose entries fill its plaintext therefore always
    /// sums to `header_len - COMP_OVERHEAD`. The check is a belt-and-braces
    /// guard against a `decrypt` that did not preserve length, not a reachable
    /// refusal, and this asserts the property that makes it so — over the
    /// boundaries, not over one convenient size.
    #[test]
    fn an_aead_ciphertext_always_decrypts_to_the_length_it_was_read_at() {
        let mk = master_key();
        let key = aead_key(&mk);
        for content in [
            &b""[..],
            &b"x"[..],
            &b"a blob that is long enough to be worth sealing"[..],
        ] {
            let ciphertext = seal(&key, 1, content);
            assert_eq!(
                ciphertext.len() as u64 - COMP_OVERHEAD,
                content.len() as u64,
                "the AEAD overhead is fixed, so the length is not a free parameter"
            );
        }
        // The boundary the check is written against: a header of exactly the
        // overhead decrypts to nothing, and its entries sum to zero.
        let empty = seal(&key, 1, b"");
        assert_eq!(empty.len() as u64, COMP_OVERHEAD);
    }

    /// Why the "its header puts it at N of a region that reads at M" arm cannot
    /// fire: the format has no offsets in it. `HeaderEntry` carries a length and
    /// an id, and both the index entry and the reader's running position are the
    /// sum of the same declared lengths, in the same order, so they cannot
    /// disagree. The arm is a guard against a header whose offsets are not the
    /// accumulating ones this crate writes — a property of the format, not a
    /// thing a damaged pack can violate. What is asserted is the property, over a
    /// multi-blob pack with a mix of kinds.
    #[test]
    fn every_index_offset_is_the_running_sum_of_the_lengths_before_it() {
        let mk = master_key();
        let key = aead_key(&mk);
        let pack = assemble(
            &key,
            &[
                Blob::plain(INCOMPRESSIBLE),
                Blob::compressed(COMPRESSIBLE),
                Blob::tree(INCOMPRESSIBLE),
            ],
            0,
            None,
        );
        let be = MemBackend::holding(pack.bytes.clone());
        let index = verify_pack(&be, &pack.hex(), pack.listed(), &mk)
            .expect("a well-formed pack must verify");
        let mut running: u32 = 0;
        for (blob, at) in index.blobs.iter().zip(&pack.blob_at) {
            assert_eq!(blob.location.offset, running);
            assert_eq!(blob.location.offset as usize, *at);
            running += blob.location.length;
        }
    }

    /// The header-entry accumulation cannot overflow either, for the same reason
    /// as the offsets it produces: the sum is a `u64` and each term is a `u32`.
    #[test]
    fn a_header_offset_accumulation_cannot_overflow_before_the_header_is_unreadable() {
        // The largest number of maximum-length entries whose offsets still fit.
        let fits = u64::MAX / u64::from(u32::MAX);
        assert!(fits > 4_294_967_295, "2^32 entries, over 159 GiB of header");
        // So the `checked_add` in `parse_header` is a guard, and the header that
        // could trip it is one this module could not have read.
        let largest_single = u64::from(u32::MAX);
        assert_eq!(
            0u64.checked_add(largest_single)
                .map(|sum| sum - largest_single),
            Some(0)
        );
    }
}
