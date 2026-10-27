//! Versioned binary codec for per-visit summary arrays, independent of the
//! SQLite schema version.
//!
//! Codec `k4na-i16x5-le/1`: five arrays (min, p10, p50, p90, max) of
//! `bin_count` signed 16-bit **little-endian** integers, concatenated in
//! that order, optionally zstd-compressed. The checksum is SHA-256 of the
//! uncompressed bytes. Readers validate the declared lengths before
//! allocating or decompressing.

use sha2::{Digest, Sha256};

use crate::StoreError;

pub const SUMMARY_CODEC: &str = "k4na-i16x5-le/1";
pub const STAT_COUNT: usize = 5;
/// Defensive bound on decoded arrays (the K4 sends 2048 bins).
pub const MAX_BINS: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Zstd(i32),
}

impl Compression {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zstd(_) => "zstd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedArrays {
    pub compression: &'static str,
    pub bin_count: u32,
    pub uncompressed_len: u32,
    pub sha256: String,
    pub data: Vec<u8>,
}

pub fn encode_stats(stats: &[Vec<i16>; STAT_COUNT], compression: Compression) -> Result<EncodedArrays, StoreError> {
    let bin_count = stats[0].len();
    if bin_count == 0 || bin_count > MAX_BINS || stats.iter().any(|s| s.len() != bin_count) {
        return Err(StoreError::Codec(format!(
            "summary arrays must share a length in 1..={MAX_BINS}"
        )));
    }
    let mut raw = Vec::with_capacity(bin_count * 2 * STAT_COUNT);
    for array in stats {
        for v in array {
            raw.extend_from_slice(&v.to_le_bytes());
        }
    }
    let sha256 = hex::encode(Sha256::digest(&raw));
    let uncompressed_len = raw.len() as u32;
    let data = match compression {
        Compression::None => raw,
        Compression::Zstd(level) => zstd::bulk::compress(&raw, level).map_err(|e| StoreError::Codec(e.to_string()))?,
    };
    Ok(EncodedArrays {
        compression: compression.name(),
        bin_count: bin_count as u32,
        uncompressed_len,
        sha256,
        data,
    })
}

pub fn decode_stats(
    codec: &str,
    compression: &str,
    bin_count: u32,
    uncompressed_len: u32,
    sha256: &str,
    data: &[u8],
) -> Result<[Vec<i16>; STAT_COUNT], StoreError> {
    if codec != SUMMARY_CODEC {
        return Err(StoreError::Codec(format!("unsupported summary codec {codec:?}")));
    }
    let bins = bin_count as usize;
    let expected = bins * 2 * STAT_COUNT;
    if bins == 0 || bins > MAX_BINS || uncompressed_len as usize != expected {
        return Err(StoreError::Codec(format!(
            "declared length {uncompressed_len} inconsistent with {bin_count} bins"
        )));
    }
    let raw = match compression {
        "none" => data.to_vec(),
        "zstd" => zstd::bulk::decompress(data, expected).map_err(|e| StoreError::Codec(format!("zstd: {e}")))?,
        other => return Err(StoreError::Codec(format!("unsupported compression {other:?}"))),
    };
    if raw.len() != expected {
        return Err(StoreError::Codec(format!(
            "decoded {} bytes, expected {expected}",
            raw.len()
        )));
    }
    if hex::encode(Sha256::digest(&raw)) != sha256 {
        return Err(StoreError::Codec("summary checksum mismatch".into()));
    }
    let (pairs, _) = raw.as_chunks::<2>();
    let mut out: [Vec<i16>; STAT_COUNT] = std::array::from_fn(|_| Vec::with_capacity(bins));
    for (i, pair) in pairs.iter().enumerate() {
        out[i / bins].push(i16::from_le_bytes(*pair));
    }
    Ok(out)
}

/// Raw frames: exact wire bytes, optionally zstd-compressed.
pub fn encode_raw(bytes: &[u8], compression: Compression) -> Result<(&'static str, Vec<u8>), StoreError> {
    match compression {
        Compression::None => Ok(("none", bytes.to_vec())),
        Compression::Zstd(level) => Ok((
            "zstd",
            zstd::bulk::compress(bytes, level).map_err(|e| StoreError::Codec(e.to_string()))?,
        )),
    }
}

pub fn decode_raw(compression: &str, byte_len: usize, data: &[u8]) -> Result<Vec<u8>, StoreError> {
    if byte_len > 1 << 20 {
        return Err(StoreError::Codec(format!("raw frame length {byte_len} too large")));
    }
    let out = match compression {
        "none" => data.to_vec(),
        "zstd" => zstd::bulk::decompress(data, byte_len).map_err(|e| StoreError::Codec(format!("zstd: {e}")))?,
        other => return Err(StoreError::Codec(format!("unsupported compression {other:?}"))),
    };
    if out.len() != byte_len {
        return Err(StoreError::Codec(format!(
            "raw frame decoded to {} bytes, expected {byte_len}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> [Vec<i16>; 5] {
        std::array::from_fn(|k| {
            (0..2048)
                .map(|i| (i as i16).wrapping_mul(7) - 1500 + k as i16)
                .collect()
        })
    }

    #[test]
    fn round_trips_both_compressions() {
        for c in [Compression::None, Compression::Zstd(3)] {
            let e = encode_stats(&sample(), c).unwrap();
            assert_eq!(e.uncompressed_len, 20_480);
            let d = decode_stats(
                SUMMARY_CODEC,
                e.compression,
                e.bin_count,
                e.uncompressed_len,
                &e.sha256,
                &e.data,
            )
            .unwrap();
            assert_eq!(d, sample());
        }
    }

    #[test]
    fn rejects_bad_lengths_codecs_and_checksums() {
        let e = encode_stats(&sample(), Compression::Zstd(3)).unwrap();
        assert!(
            decode_stats(
                "other/9",
                e.compression,
                e.bin_count,
                e.uncompressed_len,
                &e.sha256,
                &e.data
            )
            .is_err()
        );
        assert!(decode_stats(SUMMARY_CODEC, e.compression, e.bin_count, 999, &e.sha256, &e.data).is_err());
        assert!(
            decode_stats(
                SUMMARY_CODEC,
                e.compression,
                u32::MAX,
                e.uncompressed_len,
                &e.sha256,
                &e.data
            )
            .is_err()
        );
        let mut bad = e.data.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        assert!(
            decode_stats(
                SUMMARY_CODEC,
                e.compression,
                e.bin_count,
                e.uncompressed_len,
                &e.sha256,
                &bad
            )
            .is_err()
        );
        let n = encode_stats(&sample(), Compression::None).unwrap();
        let mut flipped = n.data.clone();
        flipped[10] ^= 1;
        assert!(
            decode_stats(
                SUMMARY_CODEC,
                "none",
                n.bin_count,
                n.uncompressed_len,
                &n.sha256,
                &flipped
            )
            .is_err()
        );
        let uneven: [Vec<i16>; 5] = std::array::from_fn(|k| vec![0; 10 + k]);
        assert!(encode_stats(&uneven, Compression::None).is_err());
    }

    #[test]
    fn raw_round_trip() {
        let bytes: Vec<u8> = (0..4162u32).map(|i| (i % 251) as u8).collect();
        let (c, data) = encode_raw(&bytes, Compression::Zstd(3)).unwrap();
        assert_eq!(decode_raw(c, bytes.len(), &data).unwrap(), bytes);
        assert!(decode_raw(c, bytes.len() - 1, &data).is_err());
    }
}
