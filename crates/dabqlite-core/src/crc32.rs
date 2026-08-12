//! CRC-32 (IEEE 802.3, reflected), table generated at compile time.
//!
//! Checksums are the detection half of the durability story (docs/DESIGN.md
//! §5): torn writes are *detected* end-to-end even on platforms where they
//! cannot be prevented. Hand-rolled so the core keeps zero dependencies.

const POLY: u32 = 0xEDB8_8320;

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

/// Compute the CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc = (crc >> 8) ^ TABLE[((crc ^ byte as u32) & 0xFF) as usize];
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn known_vectors() {
        // Standard check value for the IEEE polynomial.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn detects_single_bit_flip() {
        let a = crc32(b"hello world");
        let b = crc32(b"hello worle");
        assert_ne!(a, b);
    }
}
