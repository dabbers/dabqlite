//! Two-implementation cross-checking, permanent. The engine runs on the
//! schema-compiled codec; `dabqlite_core::layout::reference` is the
//! independent hand-written oracle it is forever measured against:
//!
//! - identical encode bytes for random rows,
//! - identical decode verdicts (accept/reject and decoded values) for both
//!   valid slots and arbitrary corruption,
//! - the same full-coverage property: every single-bit flip in a slot is
//!   detected, no dead bytes.

use dabqlite_core::generated::records as generated;
use dabqlite_core::layout::reference as hand;
use dabqlite_core::layout::RowKind;
use dabqlite_core::{ROW_SIZE, VALUE_LEN};
use generated::{
    decode_records_row, encode_records_row, RecordsRow, RECORDS_CRC_OFFSET, RECORDS_KIND_CHUNK,
    RECORDS_KIND_RECORD, RECORDS_KIND_TOMBSTONE, RECORDS_KIND_UPDATE, RECORDS_LEN_MAX,
    RECORDS_LEN_OFFSET, RECORDS_ROW_SIZE, RECORDS_SPAN_MAX, RECORDS_SPAN_OFFSET,
    RECORDS_SPAN_WIDTH,
};

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
    for round in 0..10_000 {
        let id = rng.next();
        let mut value = [0u8; VALUE_LEN];
        rng.fill(&mut value);
        // Both row kinds, so the discriminant byte is covered by the
        // equivalence too — not just the record path.
        let (kind, kind_byte) = match round % 4 {
            0 => (RowKind::Tombstone, RECORDS_KIND_TOMBSTONE),
            1 => (RowKind::Update, RECORDS_KIND_UPDATE),
            2 => (RowKind::Chunk, RECORDS_KIND_CHUNK),
            _ => (RowKind::Record, RECORDS_KIND_RECORD),
        };
        // Every legal payload length, cycled, with the bytes past it
        // zeroed the way a real encoder must leave them.
        let len = (round % (RECORDS_LEN_MAX as usize + 1)) as u8;
        value[len as usize..].fill(0);
        // Both settings of the continuation bit, so the byte is covered
        // whole rather than only in its low seven bits.
        let more = round % 2 == 0;

        // Every legal span, cycled, so the commit-group byte is covered by
        // the equivalence exactly like the kind byte is.
        let span = (round % (RECORDS_SPAN_MAX as usize + 1)) as u16;

        let mut hand_bytes = [0u8; ROW_SIZE];
        hand::encode_row(kind, span, len, more, id, &value, &mut hand_bytes);
        let mut gen_bytes = [0u8; RECORDS_ROW_SIZE];
        encode_records_row(
            &RecordsRow {
                kind: kind_byte,
                span,
                len,
                more,
                id,
                value,
            },
            &mut gen_bytes,
        );

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
            let kind = match round % 8 {
                0 => RowKind::Tombstone,
                2 => RowKind::Update,
                4 => RowKind::Chunk,
                _ => RowKind::Record,
            };
            let span = (round % (RECORDS_SPAN_MAX as usize + 1)) as u16;
            let len = (round % (RECORDS_LEN_MAX as usize + 1)) as u8;
            value[len as usize..].fill(0);
            let more = round % 3 == 0;
            hand::encode_row(kind, span, len, more, id, &value, &mut slot);
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
            (Some(hand_slot), Some(row)) => {
                assert_eq!(
                    (hand_slot.id, hand_slot.value),
                    (row.id, row.value),
                    "round {round}: values diverged"
                );
                let hand_kind = match hand_slot.kind {
                    RowKind::Record => RECORDS_KIND_RECORD,
                    RowKind::Tombstone => RECORDS_KIND_TOMBSTONE,
                    RowKind::Update => RECORDS_KIND_UPDATE,
                    RowKind::Chunk => RECORDS_KIND_CHUNK,
                };
                assert_eq!(
                    hand_kind, row.kind,
                    "round {round}: row KIND diverged - a record and a deletion must never be confused"
                );
                assert_eq!(
                    hand_slot.more, row.more,
                    "round {round}: the CONTINUES bit diverged - the two codecs \
                     would disagree about where a value ends"
                );
                assert_eq!(
                    hand_slot.len, row.len,
                    "round {round}: payload LEN diverged - the two codecs would \
                     disagree about how long a value is"
                );
                assert_eq!(
                    hand_slot.span, row.span,
                    "round {round}: commit SPAN diverged - the two codecs would draw \
                     commit boundaries in different places after a crash"
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
    // Both kinds: a tombstone's bytes must be as fully covered as a
    // record's, or a flip could turn a deletion back into data.
    let mut slot = [0u8; RECORDS_ROW_SIZE];
    for kind in [
        RECORDS_KIND_RECORD,
        RECORDS_KIND_TOMBSTONE,
        RECORDS_KIND_UPDATE,
        RECORDS_KIND_CHUNK,
    ] {
        let row = RecordsRow {
            kind,
            // A mid-range span: flipping any of its bits must be caught,
            // in either direction.
            span: 0b0010_1010,
            // A mid-range length too, for the same reason.
            len: 0b0000_1010,
            more: true,
            id: 0xDAB0_0001,
            value: *b"0123456789\0\0\0\0\0\0",
        };
        encode_records_row(&row, &mut slot);
        for byte in 0..RECORDS_ROW_SIZE {
            for bit in 0..8 {
                let mut damaged = slot;
                damaged[byte] ^= 1 << bit;
                assert_eq!(
                    decode_records_row(&damaged),
                    None,
                    "generated codec missed a flip at byte {byte} bit {bit} (kind {kind})"
                );
            }
        }
    }
    // And short input is rejected, not sliced.
    assert_eq!(decode_records_row(&slot[..RECORDS_ROW_SIZE - 1]), None);
}

/// A span byte beyond what the format defines is damage, and BOTH codecs
/// must refuse it — even though the checksum over the damaged row is
/// perfectly valid. This is the same rule the kind byte lives under: the
/// value space is closed, so an out-of-range value is evidence, not data.
///
/// The check matters because a too-large span would tell recovery to
/// expect a commit group longer than the engine can ever write, which is
/// exactly how a misdirected write or a foreign file would mislead it.
#[test]
fn both_codecs_refuse_a_span_the_format_does_not_define() {
    // Every illegal value the two-byte field can hold, sampled across the
    // range and exhaustively just above the boundary — the interesting
    // ones are the ones a single bit flip can reach from a legal span.
    let illegal = (RECORDS_SPAN_MAX + 1..=RECORDS_SPAN_MAX + 512).chain(
        (RECORDS_SPAN_MAX as u32 + 1..=u16::MAX as u32)
            .step_by(97)
            .map(|n| n as u16),
    );
    for span in illegal {
        let mut slot = [0u8; RECORDS_ROW_SIZE];
        // Encode a legal row, then rewrite the span byte and re-checksum
        // by hand so the slot is impeccable except for that one field.
        hand::encode_row(
            RowKind::Record,
            0,
            16,
            false,
            7,
            b"................",
            &mut slot,
        );
        slot[RECORDS_SPAN_OFFSET..RECORDS_SPAN_OFFSET + RECORDS_SPAN_WIDTH]
            .copy_from_slice(&span.to_le_bytes());
        let crc = crc32_ieee(&slot[0..RECORDS_CRC_OFFSET]);
        slot[RECORDS_CRC_OFFSET..RECORDS_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());

        assert_eq!(
            decode_records_row(&slot),
            None,
            "the generated codec accepted span {span}, which no commit can produce"
        );
        assert!(
            hand::decode_row(&slot).is_none(),
            "the reference codec accepted span {span}, which no commit can produce"
        );
    }
}

/// And every span the format DOES define round-trips through both codecs,
/// so the refusal above is a boundary and not a blanket.
#[test]
fn both_codecs_round_trip_every_legal_span() {
    for span in 0..=RECORDS_SPAN_MAX {
        let mut slot = [0u8; RECORDS_ROW_SIZE];
        hand::encode_row(
            RowKind::Update,
            span,
            16,
            false,
            99,
            b"abcdefghijklmnop",
            &mut slot,
        );
        let decoded = decode_records_row(&slot).expect("legal span must decode");
        assert_eq!(decoded.span, span);
        assert_eq!(hand::decode_row(&slot).expect("legal span").span, span);
    }
}

/// CRC-32/IEEE, spelled out here rather than imported: this test rewrites a
/// row's checksum by hand, and doing so with the crate's own helper would
/// let a bug in that helper hide the very thing under test.
fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A LEN beyond the bytes a slot can hold is damage, and both codecs must
/// refuse it even with a perfect checksum — otherwise a flip could make a
/// value claim bytes that are really its padding.
#[test]
fn both_codecs_refuse_a_len_longer_than_the_slot() {
    // Only the low seven bits are the length; the top bit is the
    // CONTINUES flag and is legal at any length. Sweep both settings of it
    // over every illegal length.
    for raw in (RECORDS_LEN_MAX as u16 + 1)..0x80 {
        for len in [raw, raw | 0x80] {
            let mut slot = [0u8; RECORDS_ROW_SIZE];
            hand::encode_row(
                RowKind::Record,
                0,
                16,
                false,
                7,
                b"................",
                &mut slot,
            );
            slot[RECORDS_LEN_OFFSET] = len as u8;
            let crc = crc32_ieee(&slot[0..RECORDS_LEN_OFFSET + 1]);
            slot[RECORDS_LEN_OFFSET + 1..RECORDS_LEN_OFFSET + 5]
                .copy_from_slice(&crc.to_le_bytes());

            assert_eq!(
                decode_records_row(&slot),
                None,
                "the generated codec accepted len {len}, longer than the slot"
            );
            assert!(
                hand::decode_row(&slot).is_none(),
                "the reference codec accepted len {len}, longer than the slot"
            );
        }
    }
}

/// And every legal length round-trips, carrying exactly those bytes back
/// — no more (the padding is not data) and no fewer.
#[test]
fn a_row_gives_back_exactly_the_bytes_its_len_claims() {
    let full = *b"0123456789abcdef";
    for len in 0..=RECORDS_LEN_MAX {
        let mut value = full;
        value[len as usize..].fill(0);
        let mut slot = [0u8; RECORDS_ROW_SIZE];
        hand::encode_row(RowKind::Record, 0, len, false, 1, &value, &mut slot);
        let decoded = hand::decode_row(&slot).expect("legal len must decode");
        assert_eq!(decoded.len, len);
        assert_eq!(
            decoded.payload(),
            &full[..len as usize],
            "payload for len {len}"
        );
        assert_eq!(decode_records_row(&slot).expect("generated").len, len);
    }
}
