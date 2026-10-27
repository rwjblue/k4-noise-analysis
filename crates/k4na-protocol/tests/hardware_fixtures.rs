// SPDX-License-Identifier: MPL-2.0
//! Replays unmodified K4 R39 captures. Expected values were extracted
//! independently with Python `struct`/`binascii` for Ham2K HaLo; they were
//! not produced by this decoder. See fixtures/k4-spectrum/README.md.

use k4na_protocol::{Decoder, PanId, crc16};
use sha2::{Digest, Sha256};

const AYB: &[u8] = include_bytes!("fixtures/k4-spectrum/k4-lan-9201-ayb.bin");
const SPANS: &[u8] = include_bytes!("fixtures/k4-spectrum/k4-lan-9201-spans-aybz.bin");
const INDICES: [usize; 9] = [0, 1, 2, 127, 511, 1023, 1024, 1535, 2047];

struct Expected {
    sequence: u8,
    pan: PanId,
    rate: u32,
    center: u64,
    noise: i32,
    crc: u16,
    bins: [i16; 9],
}

fn expected() -> Vec<Expected> {
    vec![
        Expected {
            sequence: 12,
            pan: PanId::A,
            rate: 96000,
            center: 14045000,
            noise: -1316,
            crc: 0xcb08,
            bins: [-1725, -1725, -1695, -1349, -1360, -1331, -1367, -1274, -1695],
        },
        Expected {
            sequence: 12,
            pan: PanId::Y,
            rate: 3000,
            center: 14047440,
            noise: 87,
            crc: 0x2141,
            bins: [-18, -18, -18, 63, 118, 52, -87, 52, 13],
        },
        Expected {
            sequence: 11,
            pan: PanId::B,
            rate: 96000,
            center: 7045000,
            noise: -1215,
            crc: 0xd732,
            bins: [-1545, -1545, -1545, -1248, -1246, -1152, -1192, -1191, -1515],
        },
        Expected {
            sequence: 45,
            pan: PanId::A,
            rate: 24000,
            center: 14036950,
            noise: -1392,
            crc: 0xa34f,
            bins: [-1665, -1554, -1535, -1431, -1399, -1379, -1424, -1326, -1596],
        },
        Expected {
            sequence: 45,
            pan: PanId::Y,
            rate: 3000,
            center: 14037360,
            noise: 147,
            crc: 0xcc19,
            bins: [-87, -87, -87, -6, -1, 25, 61, 104, -78],
        },
        Expected {
            sequence: 44,
            pan: PanId::B,
            rate: 96000,
            center: 14045000,
            noise: -1101,
            crc: 0x7de1,
            bins: [-1545, -1515, -1545, -1126, -1138, -1281, -1128, -1131, -1545],
        },
        Expected {
            sequence: 43,
            pan: PanId::Z,
            rate: 3000,
            center: 14037360,
            noise: 107,
            crc: 0x6887,
            bins: [-77, -77, -77, -107, 4, 32, 49, 109, -77],
        },
        Expected {
            sequence: 55,
            pan: PanId::A,
            rate: 48000,
            center: 14036950,
            noise: -1340,
            crc: 0x38fc,
            bins: [-1785, -1785, -1785, -1478, -1360, -1388, -1348, -1423, -1785],
        },
    ]
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn fixtures_are_the_documented_unmodified_captures() {
    assert_eq!(AYB.len(), 12_486);
    assert_eq!(SPANS.len(), 20_810);
    assert_eq!(
        sha256_hex(AYB),
        "c71c488ba6cbfce7d0fcac64fe4ef4a53d8932c37eac5031368f465a22090cd1"
    );
    assert_eq!(
        sha256_hex(SPANS),
        "a0b6ff7291a3940758e99e04fad5d52057659307f20de2e4208ddfb273ac0cfb"
    );
}

#[test]
fn crc_matches_independent_x25_check_value() {
    assert_eq!(crc16(b"123456789"), 0x906e);
    assert_eq!(crc16(&[]), 0);
}

#[test]
fn replays_r39_frames_with_independently_extracted_values() {
    let stream: Vec<u8> = [AYB, SPANS].concat();
    let mut decoder = Decoder::new();
    let mut frames = Vec::new();
    for chunk in stream.chunks(257) {
        decoder.push(chunk, &mut frames);
    }
    let want = expected();
    assert_eq!(frames.len(), want.len());
    for (frame, want) in frames.iter().zip(&want) {
        let h = frame.header();
        assert_eq!(h.version, 2);
        assert_eq!(h.sequence, want.sequence);
        assert_eq!(h.pan, want.pan);
        assert_eq!(h.sample_rate_hz, want.rate);
        assert_eq!(h.center_hz, want.center);
        assert_eq!(h.noise_field_tenths, want.noise);
        assert_eq!(frame.crc(), want.crc);
        for (j, &i) in INDICES.iter().enumerate() {
            assert_eq!(frame.bin(i), want.bins[j], "{} bin {i}", want.pan);
        }
    }
    // Every frame round-trips its exact wire bytes.
    let rejoined: Vec<u8> = frames.iter().flat_map(|f| f.raw().iter().copied()).collect();
    assert_eq!(rejoined, stream);
    let d = decoder.diagnostics();
    assert_eq!(decoder.pending_bytes(), 0);
    assert_eq!(d.frames_decoded, 8);
    assert_eq!(d.discarded_bytes, 0);
    assert_eq!(d.rejected_candidates(), 0);
}
