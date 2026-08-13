//! The generated codec must be bit-for-bit the codec the fault matrix
//! validated. Every guarantee in docs/FAULTS.md was proven against the
//! hand-written `dabqlite_core::layout` codec; this suite proves the
//! generated one is indistinguishable from it:
//!
//! - identical encode bytes for random rows,
//! - identical decode verdicts (accept/reject and decoded values) for both
//!   valid slots and arbitrary corruption,
//! - the same full-coverage property: every single-bit flip in a slot is
//!   detected, no dead bytes.

mod generated {
    // The generated module is a complete API surface; this test consumes
    // only the codec portion of it.
    #![allow(dead_code)]
    #![allow(clippy::all)]
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/records.rs"));
}

use dabqlite_core::layout as hand;
use dabqlite_core::{ROW_SIZE, VALUE_LEN};
use generated::{decode_records_row, encode_records_row, RecordsRow, RECORDS_ROW_SIZE};

/// Deterministic pseudo-random stream without pulling rand into this crate:
/// splitmix64, the canonical seed expander.
struct Splitmix(u64);
impl Splitmix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

#[test]
fn generated_encode_is_byte_identical_to_hand_written() {
    assert_eq!(RECORDS_ROW_SIZE, ROW_SIZE);
    let mut rng = Splitmix(1);
    for _ in 0..10_000 {
        let id = rng.next();
        let mut value = [0u8; VALUE_LEN];
        rng.fill(&mut value);

        let mut hand_bytes = [0u8; ROW_SIZE];
        hand::encode_row(id, &value, &mut hand_bytes);
        let mut gen_bytes = [0u8; RECORDS_ROW_SIZE];
        encode_records_row(&RecordsRow { id, value }, &mut gen_bytes);

        assert_eq!(
            hand_bytes, gen_bytes,
            "codec divergence for id={id}: the generated codec is not the \
             codec the fault matrix validated"
        );
    }
}

#[test]
fn generated_decode_agrees_on_valid_and_corrupt_slots() {
    let mut rng = Splitmix(2);
    for round in 0..10_000 {
        // Alternate between genuine rows (possibly corrupted) and pure noise.
        let mut slot = [0u8; ROW_SIZE];
        if round % 2 == 0 {
            let id = rng.next();
            let mut value = [0u8; VALUE_LEN];
            rng.fill(&mut value);
            hand::encode_row(id, &value, &mut slot);
            if round % 4 == 0 {
                // Corrupt a random byte with a random mask (sometimes 0 =
                // no corruption; both decoders must still agree).
                let at = (rng.next() as usize) % ROW_SIZE;
                slot[at] ^= rng.next() as u8;
            }
        } else {
            rng.fill(&mut slot);
        }

        let hand_verdict = hand::decode_row(&slot);
        let gen_verdict = decode_records_row(&slot);
        match (hand_verdict, gen_verdict) {
            (None, None) => {}
            (Some((id, value)), Some(row)) => {
                assert_eq!(
                    (id, value),
                    (row.id, row.value),
                    "round {round}: values diverged"
                );
            }
            (h, g) => panic!(
                "round {round}: verdicts diverged (hand={:?}, generated={:?})",
                h.is_some(),
                g.is_some()
            ),
        }
    }
}

#[test]
fn generated_codec_has_no_dead_bytes_either() {
    // The same exhaustive property proven for the hand codec: every single
    // bit flip anywhere in a slot must be detected.
    let mut slot = [0u8; RECORDS_ROW_SIZE];
    let row = RecordsRow {
        id: 0xDAB0_0001,
        value: *b"0123456789abcdef",
    };
    encode_records_row(&row, &mut slot);
    for byte in 0..RECORDS_ROW_SIZE {
        for bit in 0..8 {
            let mut damaged = slot;
            damaged[byte] ^= 1 << bit;
            assert_eq!(
                decode_records_row(&damaged),
                None,
                "generated codec missed a flip at byte {byte} bit {bit}"
            );
        }
    }
    // And short input is rejected, not sliced.
    assert_eq!(decode_records_row(&slot[..RECORDS_ROW_SIZE - 1]), None);
}
