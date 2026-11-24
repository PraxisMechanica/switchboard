//! Deterministic stable-Rust smoke tests exercising fuzz corpora and hostile inputs.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use switchboard::directory::{verify_peer_card, PeerCard};
use switchboard::identity::{self, LocalKeypair, Signed};
use switchboard::transport::{
    decode_frame, FrameKind, TransportError, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};

struct XorShift64(u64);

impl XorShift64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_usize(&mut self, upper: usize) -> usize {
        if upper == 0 {
            return 0;
        }
        (self.next_u64() as usize) % upper
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }
}

fn corpus_dir(target: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz")
        .join("corpus")
        .join(target)
}

fn load_corpus(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_dir(target);
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("failed to read corpus directory {}: {err}", dir.display()));

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.expect("directory entry should be readable");
        let file_type = entry
            .file_type()
            .expect("directory entry file type should be readable");
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let bytes = fs::read(entry.path())
            .unwrap_or_else(|err| panic!("failed to read corpus file {}: {err}", name));
        files.push((name, bytes));
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !files.is_empty(),
        "corpus directory {} must contain at least one seed",
        dir.display()
    );
    files
}

fn mutate_bytes(seed: &[u8], rng: &mut XorShift64) -> Vec<u8> {
    let mut buf = seed.to_vec();
    let ops = 1 + rng.next_usize(4);
    for _ in 0..ops {
        match rng.next_usize(4) {
            0 if !buf.is_empty() => {
                let idx = rng.next_usize(buf.len());
                buf[idx] ^= 1u8 << rng.next_usize(8);
            }
            1 if !buf.is_empty() => {
                let idx = rng.next_usize(buf.len());
                buf[idx] = rng.next_byte();
            }
            2 => {
                let idx = rng.next_usize(buf.len() + 1);
                buf.insert(idx, rng.next_byte());
            }
            _ if !buf.is_empty() => {
                let idx = rng.next_usize(buf.len());
                buf.remove(idx);
            }
            _ => {
                buf.push(rng.next_byte());
            }
        }
    }
    buf
}

fn frame_with_body(body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("test body fits in u32");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn exercise_verify_peer_card_bytes(data: &[u8]) {
    if let Ok(signed) = serde_json::from_slice::<Signed<PeerCard>>(data) {
        let _ = verify_peer_card(signed);
    }
}

#[test]
fn decode_frame_corpus_seeds_and_mutations_never_panic() {
    let corpus = load_corpus("decode_frame");
    assert!(
        corpus.iter().any(|(name, _)| name == "valid_knock.bin"),
        "expected valid_knock.bin seed in fuzz/corpus/decode_frame"
    );

    let mut rng = XorShift64::new(0x5eed_f00d_cafe_babe);

    for (name, bytes) in &corpus {
        let decoded = decode_frame(bytes);
        if name.starts_with("valid_") {
            let frame = decoded
                .unwrap_or_else(|err| panic!("valid seed {name} failed decode_frame: {err}"));
            assert_eq!(frame.version, PROTOCOL_VERSION);
            if name == "valid_knock.bin" {
                assert_eq!(frame.kind, FrameKind::Knock);
            }
        }

        // All prefixes/truncations must never panic.
        for cut in 0..=bytes.len() {
            let _ = decode_frame(&bytes[..cut]);
        }

        // Trailing garbage after a valid frame must fail cleanly.
        let mut with_trailing = bytes.clone();
        with_trailing.push(0x00);
        assert!(decode_frame(&with_trailing).is_err());

        // Single-byte corruptions across the seed must never panic.
        let step = (bytes.len() / 512).max(1);
        for idx in (0..bytes.len()).step_by(step) {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut mutated = bytes.clone();
                mutated[idx] ^= mask;
                let _ = decode_frame(&mutated);
            }
        }

        // Deterministic fuzzed byte mutations must never panic.
        for _ in 0..128 {
            let mutated = mutate_bytes(bytes, &mut rng);
            let _ = decode_frame(&mutated);
        }
    }

    // Explicit short, empty, malformed, unsupported-version, and oversized frames.
    let short_inputs: &[&[u8]] = &[&[], &[0], &[0, 0], &[0, 0, 0], &[0, 0, 0, 0]];
    for short in short_inputs {
        assert!(matches!(
            decode_frame(short),
            Err(TransportError::InvalidFrame(_))
        ));
    }

    let invalid_utf8 = frame_with_body(&[0xff, 0xfe, 0xfd, 0x80]);
    assert!(matches!(
        decode_frame(&invalid_utf8),
        Err(TransportError::InvalidFrame(_))
    ));

    let bad_version_body = br#"{"version":99,"kind":"Knock","payload":{}}"#;
    assert!(matches!(
        decode_frame(&frame_with_body(bad_version_body)),
        Err(TransportError::UnsupportedVersion(99))
    ));

    let over_limit =
        u32::try_from(MAX_FRAME_BYTES + 1).expect("MAX_FRAME_BYTES + 1 should fit in u32");
    let declared_over_limit = over_limit.to_be_bytes();
    assert!(matches!(
        decode_frame(&declared_over_limit),
        Err(TransportError::FrameTooLarge(_))
    ));

    let declared_u32_max = [0xff, 0xff, 0xff, 0xff, b'{', b'}'];
    assert!(matches!(
        decode_frame(&declared_u32_max),
        Err(TransportError::FrameTooLarge(_))
    ));

    let mut oversized_actual = vec![0u8; 4 + MAX_FRAME_BYTES + 1];
    oversized_actual[..4].copy_from_slice(&over_limit.to_be_bytes());
    assert!(matches!(
        decode_frame(&oversized_actual),
        Err(TransportError::FrameTooLarge(_))
    ));
}

#[test]
fn verify_peer_card_corpus_seeds_and_corruptions_never_panic() {
    let corpus = load_corpus("verify_peer_card");
    assert!(
        corpus
            .iter()
            .any(|(name, _)| name == "sample_signed_card.json"),
        "expected sample_signed_card.json seed in fuzz/corpus/verify_peer_card"
    );

    let mut rng = XorShift64::new(0xdead_beef_1234_5678);

    for (name, bytes) in &corpus {
        if name == "sample_signed_card.json" {
            let signed: Signed<PeerCard> = serde_json::from_slice(bytes)
                .expect("sample_signed_card.json must deserialize as Signed<PeerCard>");
            let verified = verify_peer_card(signed.clone())
                .expect("sample_signed_card.json must pass verify_peer_card");
            assert_eq!(verified.signer(), verified.payload().peer_id);

            // Structured JSON corruptions derived from the valid seed.
            let base: Value = serde_json::from_slice(bytes).expect("valid JSON");
            for (path, replacement) in [
                (&["payload", "handle"][..], json!("")),
                (&["payload", "handle"][..], json!("../escape")),
                (&["payload", "handle"][..], json!("UPPER")),
                (&["payload", "handle"][..], json!("a".repeat(33))),
                (&["payload", "updated_at"][..], json!(i64::MIN)),
                (&["payload", "updated_at"][..], json!(i64::MAX)),
                (&["payload", "updated_at"][..], json!("not-a-timestamp")),
                (&["payload", "direct_addrs"][..], json!(["192.0.2.1:9999"])),
                (&["payload", "peer_id"][..], json!("00".repeat(32))),
                (&["signer"][..], json!("ff".repeat(32))),
                (&["signer"][..], json!("not-hex")),
                (&["signature"][..], json!("00".repeat(64))),
                (&["signature"][..], json!("ff".repeat(64))),
                (&["signature"][..], json!("abcd")),
            ] {
                let mut mutated = base.clone();
                if let Some(slot) = mutated.pointer_mut(&format!("/{}", path.join("/"))) {
                    *slot = replacement;
                }
                let encoded = serde_json::to_vec(&mutated).expect("serialize mutated JSON");
                exercise_verify_peer_card_bytes(&encoded);
            }

            // Signer mismatch with a valid Ed25519 signature from a different keypair.
            let other_keypair = LocalKeypair::from_seed([0x42u8; 32]);
            let mismatched = identity::sign(&other_keypair, signed.payload);
            assert!(verify_peer_card(mismatched).is_err());
        } else {
            exercise_verify_peer_card_bytes(bytes);
        }

        // All prefixes/truncations must never panic.
        for cut in 0..=bytes.len() {
            exercise_verify_peer_card_bytes(&bytes[..cut]);
        }

        // Byte-level flips across the seed must never panic.
        let step = (bytes.len() / 512).max(1);
        for idx in (0..bytes.len()).step_by(step) {
            let mut mutated = bytes.clone();
            mutated[idx] ^= 0x55;
            exercise_verify_peer_card_bytes(&mutated);
        }

        // Deterministic fuzzed byte mutations must never panic.
        for _ in 0..128 {
            let mutated = mutate_bytes(bytes, &mut rng);
            exercise_verify_peer_card_bytes(&mutated);
        }
    }

    // Hostile standalone inputs must never panic.
    let hostile_inputs: &[&[u8]] = &[
        b"",
        b"{}",
        b"[]",
        b"null",
        b"\"string\"",
        b"{\"payload\":null,\"signer\":\"\",\"signature\":\"\"}",
        b"\xff\xfe\x00\x01not json",
        br#"{"payload":{"handle":"alice","peer_id":"0000000000000000000000000000000000000000000000000000000000000000","direct_addrs":[],"extensions":[],"updated_at":0},"signer":"0000000000000000000000000000000000000000000000000000000000000000","signature":"00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}"#,
    ];
    for input in hostile_inputs {
        exercise_verify_peer_card_bytes(input);
    }
}
