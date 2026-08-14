//! The migration path (docs/DESIGN.md §4.8): pure `OldRow -> NewRow`
//! functions over generated types.
//!
//! A migration runs inside the NEW binary, offline, single-writer. The
//! function here is deliberately trivial to state and total by
//! construction: every value the old type can hold maps to exactly one
//! value of the new type, no `Option`, no `Result`, no panic path. That
//! totality is what makes "the migration cannot fail halfway through a
//! row" a type-level fact instead of a hope — and the property tests
//! below re-verify it over the old type's entire value-space structure
//! (every byte pattern of the fixed-width payload, boundary ids).
//!
//! Widening policy (§4.8 field discipline: append, never reorder, never
//! remove): v1's 8-byte `value` keeps its offset and its bytes; the new
//! tail is zero-filled. A v2 reader sees old payloads left-aligned with a
//! zeroed suffix — deterministic, order-preserving, reversible up to the
//! (zero) tail.

use crate::generated::{records, records_v1};

/// v1 row width on disk (24 bytes: id, 8-byte value, crc, padding).
pub const V1_ROW_SIZE: usize = records_v1::RECORDS_ROW_SIZE;
/// The legacy schema's hash, as it appears in a v1 file's superblock.
pub const V1_SCHEMA_HASH: u64 = records_v1::RECORDS_SCHEMA_HASH;
/// v1 payload width.
pub const V1_VALUE_LEN: usize = 8;

const _: () = assert!(V1_ROW_SIZE == 24);
// The whole point of the gate: the two schemas must never collide.
const _: () = assert!(V1_SCHEMA_HASH != records::RECORDS_SCHEMA_HASH);

/// The pure migration: total over every v1 row, by construction.
pub fn migrate_row(old: records_v1::RecordsRow) -> records::RecordsRow {
    let mut value = [0u8; 16];
    value[..V1_VALUE_LEN].copy_from_slice(&old.value);
    records::RecordsRow { id: old.id, value }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structured sweep of the old type's value space: boundary ids ×
    /// per-byte bit patterns. Totality means every one maps, and the map
    /// preserves id, preserves payload bytes at their offsets, and
    /// zero-fills exactly the appended tail.
    #[test]
    fn migration_is_total_and_shape_preserving() {
        let ids = [0u64, 1, u64::MAX, u64::MAX - 1, 0x8000_0000_0000_0000];
        for &id in &ids {
            for pattern in 0..=255u8 {
                for hot in 0..V1_VALUE_LEN {
                    let mut value = [pattern; V1_VALUE_LEN];
                    value[hot] = !pattern;
                    let new = migrate_row(records_v1::RecordsRow { id, value });
                    assert_eq!(new.id, id);
                    assert_eq!(&new.value[..V1_VALUE_LEN], &value);
                    assert_eq!(new.value[V1_VALUE_LEN..], [0u8; 8]);
                }
            }
        }
    }

    /// Every checksum-valid v1 SLOT migrates into a checksum-valid v2
    /// slot: the full disk-to-disk pipeline (decode v1, migrate, encode
    /// v2, decode v2) round-trips, seeded-randomly over the value space.
    #[test]
    fn valid_v1_slots_migrate_to_valid_v2_slots() {
        // Deterministic LCG; no ambient randomness in core tests either.
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        for _ in 0..10_000 {
            let id = next();
            let value = next().to_le_bytes();
            let old = records_v1::RecordsRow { id, value };
            let mut v1_slot = [0u8; V1_ROW_SIZE];
            records_v1::encode_records_row(&old, &mut v1_slot);
            let decoded = records_v1::decode_records_row(&v1_slot).expect("valid v1 slot");
            let new = migrate_row(decoded);
            let mut v2_slot = [0u8; records::RECORDS_ROW_SIZE];
            records::encode_records_row(&new, &mut v2_slot);
            let back = records::decode_records_row(&v2_slot).expect("valid v2 slot");
            assert_eq!(back.id, id);
            assert_eq!(&back.value[..V1_VALUE_LEN], &value);
        }
    }

    /// Order preservation: migration never reorders or renumbers — the
    /// btree rebuild after migration must see the same key sequence.
    #[test]
    fn migration_preserves_key_order() {
        let keys = [0u64, 1, 2, 100, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        let migrated: alloc::vec::Vec<u64> = keys
            .iter()
            .map(|&id| {
                migrate_row(records_v1::RecordsRow {
                    id,
                    value: id.to_le_bytes(),
                })
                .id
            })
            .collect();
        assert_eq!(&migrated[..], &keys[..]);
    }
}
