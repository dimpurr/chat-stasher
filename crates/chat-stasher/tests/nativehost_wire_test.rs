//! Native Messaging framing (`contracts/nativehost-protocol.md` §2).
//!
//! These tests drive the codec directly, without a process, so the properties
//! that matter most can be asserted precisely rather than inferred from a
//! subprocess's behaviour:
//!
//! * a length prefix over the cap must be refused **without reading the body** —
//!   proven here by a reader that returns an error if it is asked for anything
//!   past the prefix, not inferred from a peer that happens to have closed;
//! * a frame that arrives in pieces must reassemble — a pipe is allowed to
//!   return short reads, and a codec that assumed one read per chunk would work
//!   on every developer's machine and fail under load;
//! * EOF inside the header and EOF inside the body are the same outcome to the
//!   caller (nothing is written), and both must stay distinct from a complete
//!   zero-length frame.
//!
//! Every request used here is one the host can refuse **without consulting the
//! config** (a bad protocol version, an unknown type, a non-request). A `hello`
//! or a `deliver` would make `respond` load `~/.config/chat-stasher/config.toml`,
//! and a unit test has no business reading a real user's config file — those
//! paths are exercised end to end, with `XDG_CONFIG_HOME` pointed at a temp dir.

use chat_stasher::nativehost::{
    encode_response_frame, read_request_frame, respond, serve_one, HostOutcome, NackKind,
    RequestFrame, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// A global allocator that records how many bytes were *requested*.
///
/// "The length prefix is checked before allocating" is a claim about `vec!`,
/// not about `read`, and no reader can observe it: a mutation that allocates
/// the body and then refuses it reads exactly the same number of bytes. The
/// only way to assert the property is to watch the allocator, so this test
/// binary does.
struct CountingAllocator;

static REQUESTED_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        REQUESTED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // `vec![0u8; n]` goes through this one, which is exactly the call the
        // length-prefix check exists to avoid making.
        REQUESTED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            REQUESTED_BYTES.fetch_add(new_size - layout.size(), Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The two tests that measure the allocator must not overlap: `cargo test` runs
/// the tests of one binary on several threads, and a sibling test that
/// legitimately allocates a whole 64 MiB body would otherwise land inside this
/// measurement.
static ALLOCATOR_MEASUREMENT: Mutex<()> = Mutex::new(());

/// Take the measurement lock, ignoring poisoning.
///
/// A poisoning-aware lock would make one test's failure cascade into a second
/// failure with a message about a mutex, which reads like two bugs and is one.
fn lock_measurement() -> std::sync::MutexGuard<'static, ()> {
    ALLOCATOR_MEASUREMENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn bytes_requested_since(before: usize) -> usize {
    REQUESTED_BYTES
        .load(Ordering::Relaxed)
        .saturating_sub(before)
}

/// A reader that yields exactly the length prefix and then *fails* instead of
/// reporting EOF. Reading past the prefix is therefore an observable error
/// rather than a silent `Ok(0)`, which is what makes "the body was not read" a
/// real assertion rather than a coincidence of the peer having closed.
struct PrefixThenPoison {
    prefix: [u8; 4],
    served: usize,
}

impl Read for PrefixThenPoison {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        assert!(
            self.served < self.prefix.len(),
            "the reader was asked for the body of a frame it should have refused"
        );
        let remaining = &self.prefix[self.served..];
        let take = remaining.len().min(buf.len());
        buf[..take].copy_from_slice(&remaining[..take]);
        self.served += take;
        Ok(take)
    }
}

/// A reader that hands out one byte per call, whatever it was asked for.
struct OneByteAtATime {
    bytes: Vec<u8>,
    served: usize,
}

impl Read for OneByteAtATime {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.served >= self.bytes.len() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.bytes[self.served];
        self.served += 1;
        Ok(1)
    }
}

fn framed(body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u32).to_ne_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

fn request(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("serialise request")
}

#[test]
fn a_response_frame_round_trips_through_the_same_framing() {
    // Protocol 2 is refused before the config is ever opened, which keeps this
    // a pure codec test.
    let response = respond(&request(
        serde_json::json!({"protocol": 2, "type": "hello"}),
    ));
    let encoded = encode_response_frame(&response).expect("encode");

    // Re-read our own response frame with the request reader: the framing is
    // the browser's on both sides, so it has to be symmetric.
    let mut cursor = io::Cursor::new(encoded);
    match read_request_frame(&mut cursor).expect("read back") {
        RequestFrame::Frame(body) => {
            let decoded: serde_json::Value = serde_json::from_slice(&body).expect("parse");
            assert_eq!(decoded, response);
        }
        other => panic!("expected a complete frame, got {other:?}"),
    }
}

#[test]
fn a_zero_length_frame_is_a_frame_and_not_an_eof() {
    let mut cursor = io::Cursor::new(framed(b""));
    match read_request_frame(&mut cursor).expect("read") {
        RequestFrame::Frame(body) => assert!(body.is_empty(), "zero-length body expected"),
        other => panic!("a zero-length frame must still be a frame, got {other:?}"),
    }
    // ...and an empty body is not a JSON request, so the answer is a nack whose
    // request_id is null because there was nothing to read one from.
    let response = respond(b"");
    assert_eq!(response["type"], "nack");
    assert_eq!(response["kind"], "bad-request");
    assert!(
        response["request_id"].is_null(),
        "an unreadable request_id must be null, not an empty string: {response}"
    );
}

#[test]
fn a_frame_that_arrives_one_byte_at_a_time_reassembles() {
    let body = br#"{"protocol":2,"type":"hello"}"#;
    let mut reader = OneByteAtATime {
        bytes: framed(body),
        served: 0,
    };
    match read_request_frame(&mut reader).expect("read") {
        RequestFrame::Frame(read) => assert_eq!(read, body),
        other => panic!("a split frame must reassemble, got {other:?}"),
    }
}

#[test]
fn a_prefix_over_the_cap_is_refused_without_reading_the_body() {
    let declared = MAX_REQUEST_BYTES + 1;
    let mut reader = PrefixThenPoison {
        prefix: (declared as u32).to_ne_bytes(),
        served: 0,
    };
    match read_request_frame(&mut reader).expect("read") {
        RequestFrame::TooLarge { declared: seen } => assert_eq!(seen, declared),
        other => panic!("an oversized prefix must not be read as a frame, got {other:?}"),
    }
    assert_eq!(
        reader.served, 4,
        "exactly the length prefix may be consumed; the body must not be read"
    );
}

/// The other half of the same rule: the body must not be **allocated** either.
///
/// A prefix of `u32::MAX` is used rather than "the cap plus one" so the
/// difference between the two behaviours is four gigabytes rather than thirty
/// bytes — impossible to confuse with the ambient allocations of a test
/// process.
#[test]
fn a_prefix_over_the_cap_allocates_nothing_for_the_body() {
    let _guard = lock_measurement();
    let declared = u64::from(u32::MAX);
    let mut reader = PrefixThenPoison {
        prefix: (declared as u32).to_ne_bytes(),
        served: 0,
    };

    let before = REQUESTED_BYTES.load(Ordering::Relaxed);
    match read_request_frame(&mut reader).expect("read") {
        RequestFrame::TooLarge { declared: seen } => assert_eq!(seen, declared),
        other => panic!("an oversized prefix must not be read as a frame, got {other:?}"),
    }
    let requested = bytes_requested_since(before);
    // The bound is coarse on purpose. Tests beside this one run on other
    // threads and allocate (the measured ambient noise is on the order of a
    // kilobyte); the failure this guards against allocates the *claimed body*,
    // four gigabytes, so a 32 MiB line separates the two by more than two
    // orders of magnitude in both directions.
    assert!(
        requested < 32 * 1024 * 1024,
        "the codec asked the allocator for {requested} bytes while refusing a {declared} byte body"
    );
    assert_eq!(reader.served, 4);
}

#[test]
fn a_prefix_exactly_at_the_cap_is_still_accepted() {
    let _guard = lock_measurement();
    // The cap is "larger than 64 MiB is refused", so 64 MiB itself is legal.
    // The body is not supplied, which is what makes this cheap: the point is
    // that the *prefix* passed the size gate, so the codec then reported a
    // truncated body rather than a size refusal.
    let mut cursor = io::Cursor::new((MAX_REQUEST_BYTES as u32).to_ne_bytes().to_vec());
    assert_eq!(
        read_request_frame(&mut cursor).expect("read"),
        RequestFrame::Truncated,
        "an at-the-cap prefix must get past the size gate"
    );
}

#[test]
fn an_eof_inside_the_header_is_truncated() {
    // Nothing at all.
    assert_eq!(
        read_request_frame(&mut io::Cursor::new(Vec::new())).expect("read"),
        RequestFrame::Truncated
    );
    // Half a header.
    assert_eq!(
        read_request_frame(&mut io::Cursor::new(vec![0x01, 0x02])).expect("read"),
        RequestFrame::Truncated
    );
    // One byte short of a header.
    assert_eq!(
        read_request_frame(&mut io::Cursor::new(vec![0x01, 0x02, 0x03])).expect("read"),
        RequestFrame::Truncated
    );
}

#[test]
fn an_eof_inside_the_body_is_truncated() {
    let mut bytes = (10u32).to_ne_bytes().to_vec();
    bytes.extend_from_slice(b"{\"proto"); // 7 of the 10 declared bytes
    assert_eq!(
        read_request_frame(&mut io::Cursor::new(bytes)).expect("read"),
        RequestFrame::Truncated
    );
}

#[test]
fn a_truncated_frame_produces_no_output() {
    // The whole stdout discipline in one assertion: the writer must not have
    // been touched at all.
    let mut reader = io::Cursor::new(vec![0x00, 0x00]);
    let mut stdout: Vec<u8> = Vec::new();
    let outcome = serve_one(&mut reader, &mut stdout).expect("serve");
    assert_eq!(outcome, HostOutcome::Unreadable);
    assert!(
        stdout.is_empty(),
        "an unreadable frame must leave stdout byte-empty, got {stdout:?}"
    );
}

#[test]
fn an_oversized_prefix_is_answered_with_a_too_large_nack() {
    // The same refusal, through the full serve path: the prefix is answered,
    // and nothing beyond it is ever read.
    let mut reader = io::Cursor::new(((MAX_REQUEST_BYTES + 1) as u32).to_ne_bytes().to_vec());
    let mut stdout: Vec<u8> = Vec::new();
    assert_eq!(
        serve_one(&mut reader, &mut stdout).expect("serve"),
        HostOutcome::Answered
    );
    let mut cursor = io::Cursor::new(stdout);
    match read_request_frame(&mut cursor).expect("read back") {
        RequestFrame::Frame(body) => {
            let decoded: serde_json::Value = serde_json::from_slice(&body).expect("parse");
            assert_eq!(decoded["type"], "nack");
            assert_eq!(decoded["kind"], "too-large");
            assert_eq!(decoded["retryable"], false);
        }
        other => panic!("expected a nack frame, got {other:?}"),
    }
}

#[test]
fn the_nack_kind_vocabulary_matches_the_contract_table() {
    // §6.3, both columns. Pinned here because the host maps internal outcomes
    // onto these words, and a typo would stay invisible until the extension on
    // the other side implemented against it.
    let table = [
        (NackKind::ProtocolVersion, "protocol-version", false),
        (NackKind::BadRequest, "bad-request", false),
        (NackKind::TooLarge, "too-large", false),
        (NackKind::Integrity, "integrity", true),
        (NackKind::InvalidBundle, "invalid-bundle", false),
        (NackKind::Config, "config", false),
        (NackKind::StageUnavailable, "stage-unavailable", true),
        (NackKind::Io, "io", true),
    ];
    for (kind, slug, retryable) in table {
        assert_eq!(kind.slug(), slug);
        assert_eq!(kind.retryable(), retryable, "{slug} retryable flag");
    }
}

#[test]
fn a_protocol_version_nack_advertises_the_supported_list() {
    let response = respond(&request(
        serde_json::json!({"protocol": 7, "type": "deliver"}),
    ));
    assert_eq!(response["kind"], "protocol-version");
    assert_eq!(response["supported"], serde_json::json!([1]));
    assert_eq!(response["retryable"], false);
}

#[test]
fn a_missing_protocol_is_a_protocol_version_nack_not_a_bad_request() {
    // §6.3 puts "missing or not supported" in one row, so an absent `protocol`
    // and an unsupported one must not be two different answers.
    let response = respond(&request(serde_json::json!({"type": "hello"})));
    assert_eq!(response["kind"], "protocol-version");
    assert_eq!(response["supported"], serde_json::json!([1]));
}

#[test]
fn a_malformed_request_id_is_null_in_the_nack_rather_than_echoed() {
    // Echoing it would produce a nack that itself fails schema validation,
    // because the schema constrains `request_id` to the same pattern.
    let response = respond(&request(serde_json::json!({
        "protocol": 1,
        "type": "deliver",
        "request_id": "not a valid id!",
    })));
    assert_eq!(response["kind"], "bad-request");
    assert!(
        response["request_id"].is_null(),
        "an id outside the schema pattern must not be echoed: {response}"
    );
}

#[test]
fn an_oversized_response_becomes_a_small_nack_rather_than_a_truncated_frame() {
    // A truncated frame is unparseable, which the reader cannot tell apart from
    // a crashed host, so the encoder must never write one.
    let huge = serde_json::json!({
        "protocol": 1,
        "type": "nack",
        "request_id": null,
        "kind": "io",
        "retryable": true,
        "detail": "x".repeat(MAX_RESPONSE_BYTES),
    });
    let encoded = encode_response_frame(&huge).expect("encode");
    assert!(
        encoded.len() <= MAX_RESPONSE_BYTES,
        "the encoded frame is {} bytes, over the {MAX_RESPONSE_BYTES} cap",
        encoded.len()
    );
    let mut cursor = io::Cursor::new(encoded);
    match read_request_frame(&mut cursor).expect("read back") {
        RequestFrame::Frame(body) => {
            let decoded: serde_json::Value = serde_json::from_slice(&body).expect("parse");
            assert_eq!(decoded["type"], "nack");
            assert_eq!(decoded["kind"], "io");
        }
        other => panic!("the replacement must be a readable frame, got {other:?}"),
    }
}

#[test]
fn a_long_detail_is_truncated_to_the_contract_cap_and_stays_valid_utf8() {
    // 4 KiB is measured in bytes, so a detail made of multi-byte characters is
    // exactly the case where a naive truncation splits a character. The unknown
    // message type is echoed into the detail, which is how the length is
    // produced without opening a config file.
    let long_type = "é".repeat(10_000);
    let response = respond(&request(
        serde_json::json!({"protocol": 1, "type": long_type}),
    ));
    assert_eq!(response["kind"], "bad-request");
    let detail = response["detail"].as_str().expect("detail is a string");
    assert!(
        detail.len() <= 4096,
        "detail is {} bytes, over the 4096 cap",
        detail.len()
    );
    assert!(detail.ends_with("[truncated]"), "detail: {detail:?}");
    // The marker survived *and* the string is still a `str`, which is only
    // possible if the cut landed on a character boundary.
    assert!(detail.is_char_boundary(detail.len()));
}
