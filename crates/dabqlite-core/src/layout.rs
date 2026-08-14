//! On-disk layout: fixed-width rows and the superblock copy set.
//!
//! Layout is fixed at compile time (docs/DESIGN.md §4.2): field offsets,
//! record widths, and the file set are constants. Records are memcpy-able —
//! no deserialization, just checksum verification.
//!
//! ## Row slot (32 bytes)
//!
//! ```text
//! offset  size  field
//!      0     8  id        (u64 LE)
//!      8    16  value     (fixed-width payload)
//!     24     4  crc32     (over bytes 0..24)
//!     28     4  padding   (zero)
//! ```
//!
//! ## Superblock copy (64 bytes, SB_COPIES redundant slots in the zone)
//!
//! ```text
//! offset  size  field
//!      0     8  magic       "DABQSB01"
//!      8     8  generation  (u64 LE, monotonic; the atomicity point)
//!     16     8  row_count   (u64 LE, authoritative committed row count)
//!     24     8  schema_hash (u64 LE; an old binary opening a new file fails
//!                            at startup instead of misreading offsets, §4.8)
//!     32     4  crc32       (over bytes 0..32)
//!     36    28  padding     (zero)
//! ```

use crate::crc32::crc32;
use crate::generated::records;

/// Width of the single value field in the vertical slice.
pub const VALUE_LEN: usize = 16;
/// Fixed width of one row slot, derived from the compiled schema.
pub const ROW_SIZE: usize = records::RECORDS_ROW_SIZE;

// The hand-written constants below must agree with the schema-compiled
// layout; a schema edit that changes any of these fails right here, at
// compile time, before a single test runs.
const _: () = assert!(ROW_SIZE == 32);
const _: () = assert!(records::RECORDS_COL_ID_OFFSET == 0);
const _: () = assert!(records::RECORDS_COL_VALUE_OFFSET == 8);
const _: () = assert!(records::RECORDS_CRC_OFFSET == 8 + VALUE_LEN);
const _: () = assert!(VALUE_LEN == records::RECORDS_CRC_OFFSET - records::RECORDS_COL_VALUE_OFFSET);
/// Fixed width of one superblock copy.
pub const SB_COPY_SIZE: usize = 64;
/// Number of redundant superblock copies (docs/DESIGN.md §4.4). Commits
/// rotate through the slots, writing the stale one; a torn write can only
/// corrupt the slot being written, so the previous generation always
/// survives.
pub const SB_COPIES: usize = 4;
/// Total size of the superblock zone.
pub const SB_ZONE_SIZE: usize = SB_COPY_SIZE * SB_COPIES;

/// Magic bytes identifying a superblock copy.
pub const SB_MAGIC: [u8; 8] = *b"DABQSB01";

/// Hash of the compiled schema, derived by `dabqlite-codegen` from
/// `schema/records.sql` (FNV-1a 64 over the canonical schema rendering).
/// A schema change that alters layout changes this value and fails the
/// codegen pin tests until the migration story is consciously handled
/// (docs/DESIGN.md §4.8).
pub const SCHEMA_HASH: u64 = records::RECORDS_SCHEMA_HASH;

/// Encode a row into its slot. Delegates to the schema-compiled codec; the
/// hand-written [`reference`] implementation exists as a permanent second
/// opinion and is asserted equivalent in debug builds and test suites.
pub fn encode_row(id: u64, value: &[u8; VALUE_LEN], out: &mut [u8; ROW_SIZE]) {
    records::encode_records_row(&records::RecordsRow { id, value: *value }, out);
    // Pair assertion (docs/DESIGN.md §7.4): the independent reference codec
    // must agree byte-for-byte with the generated one.
    #[cfg(debug_assertions)]
    {
        let mut check = [0u8; ROW_SIZE];
        reference::encode_row(id, value, &mut check);
        debug_assert_eq!(*out, check, "generated and reference codecs diverged");
    }
}

/// Decode and verify a row slot. Returns `None` if the slot is too short,
/// fails its checksum, or has damaged padding. Padding is validated so that
/// *every* byte of a live slot is covered: a single-bit flip anywhere in a
/// committed row must be detectable, with no dead zones.
pub fn decode_row(bytes: &[u8]) -> Option<(u64, [u8; VALUE_LEN])> {
    let decoded = records::decode_records_row(bytes).map(|row| (row.id, row.value));
    debug_assert_eq!(
        decoded,
        reference::decode_row(bytes),
        "generated and reference codecs diverged on decode"
    );
    decoded
}

/// The hand-written row codec, kept as an independent oracle for the
/// schema-compiled one (two implementations, permanently cross-checked:
/// in debug builds on every call above, and exhaustively in the codegen
/// equivalence suite). Never wired into the engine directly.
pub mod reference {
    use super::{crc32, ROW_SIZE, VALUE_LEN};

    pub fn encode_row(id: u64, value: &[u8; VALUE_LEN], out: &mut [u8; ROW_SIZE]) {
        out[0..8].copy_from_slice(&id.to_le_bytes());
        out[8..8 + VALUE_LEN].copy_from_slice(value);
        let crc = crc32(&out[0..24]);
        out[24..28].copy_from_slice(&crc.to_le_bytes());
        out[28..32].fill(0);
    }

    pub fn decode_row(bytes: &[u8]) -> Option<(u64, [u8; VALUE_LEN])> {
        if bytes.len() < ROW_SIZE {
            return None;
        }
        let stored = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
        if crc32(&bytes[0..24]) != stored {
            return None;
        }
        if bytes[28..32] != [0u8; 4] {
            return None;
        }
        let id = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let value: [u8; VALUE_LEN] = bytes[8..8 + VALUE_LEN].try_into().ok()?;
        Some((id, value))
    }
}

/// A decoded, checksum-valid superblock copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SbCopy {
    pub generation: u64,
    pub row_count: u64,
}

/// Why a superblock copy was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbDecodeError {
    /// Wrong magic, bad checksum, or short read: not a valid copy at all.
    /// Expected for stale/torn slots; recovery just skips it.
    Invalid,
    /// Structurally valid but written by a different schema. Must abort the
    /// open — field offsets cannot be trusted (docs/DESIGN.md §4.8).
    SchemaMismatch { file_schema: u64 },
}

/// Encode a superblock copy into a 64-byte slot.
pub fn encode_sb(generation: u64, row_count: u64, out: &mut [u8; SB_COPY_SIZE]) {
    // Negative-space assertion: generation 0 is reserved as "never written".
    debug_assert!(generation > 0, "superblock generation must be positive");
    out[0..8].copy_from_slice(&SB_MAGIC);
    out[8..16].copy_from_slice(&generation.to_le_bytes());
    out[16..24].copy_from_slice(&row_count.to_le_bytes());
    out[24..32].copy_from_slice(&SCHEMA_HASH.to_le_bytes());
    let crc = crc32(&out[0..32]);
    out[32..36].copy_from_slice(&crc.to_le_bytes());
    out[36..].fill(0);
    // Pair assertion: encode/decode roundtrip.
    debug_assert!(matches!(
        decode_sb(out),
        Ok(c) if c.generation == generation && c.row_count == row_count
    ));
}

/// Decode and verify a superblock copy.
pub fn decode_sb(bytes: &[u8]) -> Result<SbCopy, SbDecodeError> {
    if bytes.len() < SB_COPY_SIZE {
        return Err(SbDecodeError::Invalid);
    }
    if bytes[0..8] != SB_MAGIC {
        return Err(SbDecodeError::Invalid);
    }
    let stored = u32::from_le_bytes(bytes[32..36].try_into().expect("fixed slice"));
    if crc32(&bytes[0..32]) != stored {
        return Err(SbDecodeError::Invalid);
    }
    // Padding validated for full-slot coverage: no byte of a superblock
    // copy is exempt from corruption detection.
    if bytes[36..SB_COPY_SIZE] != [0u8; SB_COPY_SIZE - 36] {
        return Err(SbDecodeError::Invalid);
    }
    let file_schema = u64::from_le_bytes(bytes[24..32].try_into().expect("fixed slice"));
    if file_schema != SCHEMA_HASH {
        return Err(SbDecodeError::SchemaMismatch { file_schema });
    }
    Ok(SbCopy {
        generation: u64::from_le_bytes(bytes[8..16].try_into().expect("fixed slice")),
        row_count: u64::from_le_bytes(bytes[16..24].try_into().expect("fixed slice")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_roundtrip() {
        let mut slot = [0u8; ROW_SIZE];
        let value = *b"0123456789abcdef";
        encode_row(42, &value, &mut slot);
        assert_eq!(decode_row(&slot), Some((42, value)));
    }

    #[test]
    fn row_rejects_corruption() {
        let mut slot = [0u8; ROW_SIZE];
        encode_row(42, &[7u8; VALUE_LEN], &mut slot);
        slot[3] ^= 0x01;
        assert_eq!(decode_row(&slot), None);
        // Negative space: an all-zero slot must not decode.
        assert_eq!(decode_row(&[0u8; ROW_SIZE]), None);
    }

    #[test]
    fn every_single_bit_flip_in_a_slot_is_detected() {
        // Full-coverage guarantee: no byte of a row or superblock copy is a
        // dead zone. Flip every bit of every byte, one at a time.
        let mut row = [0u8; ROW_SIZE];
        encode_row(42, &[7u8; VALUE_LEN], &mut row);
        for byte in 0..ROW_SIZE {
            for bit in 0..8 {
                let mut damaged = row;
                damaged[byte] ^= 1 << bit;
                assert_eq!(
                    decode_row(&damaged),
                    None,
                    "row flip at byte {byte} bit {bit} undetected"
                );
            }
        }
        let mut sb = [0u8; SB_COPY_SIZE];
        encode_sb(7, 123, &mut sb);
        for byte in 0..SB_COPY_SIZE {
            for bit in 0..8 {
                let mut damaged = sb;
                damaged[byte] ^= 1 << bit;
                assert!(
                    decode_sb(&damaged).is_err(),
                    "superblock flip at byte {byte} bit {bit} undetected"
                );
            }
        }
    }

    #[test]
    fn sb_roundtrip() {
        let mut slot = [0u8; SB_COPY_SIZE];
        encode_sb(7, 123, &mut slot);
        assert_eq!(
            decode_sb(&slot),
            Ok(SbCopy {
                generation: 7,
                row_count: 123
            })
        );
    }

    #[test]
    fn sb_rejects_torn_write() {
        let mut slot = [0u8; SB_COPY_SIZE];
        encode_sb(7, 123, &mut slot);
        // A torn write: only the first 20 bytes reached disk.
        let mut torn = [0u8; SB_COPY_SIZE];
        torn[..20].copy_from_slice(&slot[..20]);
        assert_eq!(decode_sb(&torn), Err(SbDecodeError::Invalid));
    }

    #[test]
    fn sb_rejects_schema_mismatch() {
        let mut slot = [0u8; SB_COPY_SIZE];
        encode_sb(7, 123, &mut slot);
        // Rewrite the schema hash and fix up the checksum: structurally
        // valid, wrong schema.
        slot[24..32].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        let crc = crc32(&slot[0..32]);
        slot[32..36].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            decode_sb(&slot),
            Err(SbDecodeError::SchemaMismatch {
                file_schema: 0xDEAD_BEEF
            })
        );
    }
}
