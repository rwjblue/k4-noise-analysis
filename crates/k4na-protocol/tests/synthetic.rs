// SPDX-License-Identifier: MPL-2.0
//! Synthetic framing tests (ported from HaLo's codec tests and extended).
//! These packets are NOT hardware evidence; they exercise corruption,
//! chunking and resynchronization paths.

use k4na_protocol::synth::{fix_crc, fix_header_sum};
use k4na_protocol::{Decoder, FRAME_LEN, PanId, SpectrumFrame};
use proptest::prelude::*;

/// Builds a frame from raw ASCII header fields, sealing header sum and CRC.
fn packet(sequence: u8, pan: u8, version: u8, rate: &str, center: &str, noise: &str, levels: &[i16]) -> Vec<u8> {
    let mut b = vec![0u8; FRAME_LEN];
    b[..7].copy_from_slice(&[0xff, 0xfe, 0x01, 0x00, version, sequence, pan]);
    b[7..13].copy_from_slice(rate.as_bytes());
    b[13..24].copy_from_slice(center.as_bytes());
    b[24..29].copy_from_slice(noise.as_bytes());
    for i in 0..2048 {
        let v = levels.get(i).copied().unwrap_or(-1200);
        b[64 + 2 * i..66 + 2 * i].copy_from_slice(&v.to_be_bytes());
    }
    fix_header_sum(&mut b);
    fix_crc(&mut b);
    b
}

fn simple(sequence: u8) -> Vec<u8> {
    packet(sequence, b'A', 2, "192000", "00014074000", "-1234", &[])
}

fn with_pan(sequence: u8, pan: u8) -> Vec<u8> {
    packet(sequence, pan, 2, "192000", "00014074000", "-1234", &[])
}

#[test]
fn decodes_full_signed_range_and_preserves_header() {
    let bytes = packet(
        17,
        b'A',
        2,
        "192000",
        "00014074000",
        "-1234",
        &[-32768, -1234, -1, 0, 1, 1234, 32767],
    );
    let frames = Decoder::new().decode(&bytes);
    let f = &frames[0];
    assert_eq!(f.header().sequence, 17);
    assert_eq!(f.pan(), PanId::A);
    assert_eq!(f.header().sample_rate_hz, 192_000);
    assert_eq!(f.header().center_hz, 14_074_000);
    assert_eq!(f.header().noise_field_tenths, -1234);
    let bins = f.bins();
    assert_eq!(&bins[..7], &[-32768, -1234, -1, 0, 1, 1234, 32767]);
    assert_eq!(bins[2047], -1200);
    assert_eq!(f.header_bytes(), &bytes[..64]);
    assert_eq!(&f.raw()[..], &bytes[..]);
}

#[test]
fn accepts_fixed_width_padding_and_signs() {
    let stream = [
        packet(1, b'A', 2, " +9600", " +14074000 ", " -123", &[]),
        packet(2, b'A', 2, "999999", "00000000000", "+0012", &[]),
        packet(3, b'A', 2, "12000 ", "14074000   ", "123  ", &[]),
    ]
    .concat();
    let frames = Decoder::new().decode(&stream);
    let rates: Vec<_> = frames.iter().map(|f| f.header().sample_rate_hz).collect();
    let centers: Vec<_> = frames.iter().map(|f| f.header().center_hz).collect();
    let noise: Vec<_> = frames.iter().map(|f| f.header().noise_field_tenths).collect();
    assert_eq!(rates, [9600, 999_999, 12000]);
    assert_eq!(centers, [14_074_000, 0, 14_074_000]);
    assert_eq!(noise, [-123, 12, 123]);
}

#[test]
fn chunk_boundaries_have_no_meaning() {
    let bytes = simple(17);
    let mut splits: Vec<usize> = (0..=65).collect();
    splits.extend([127, 2048, 4159, 4160, 4161]);
    for split in splits {
        let mut d = Decoder::new();
        assert!(d.decode(&bytes[..split]).is_empty(), "split {split}");
        assert_eq!(d.decode(&bytes[split..]).len(), 1, "split {split}");
        assert_eq!(d.pending_bytes(), 0);
    }
    let mut d = Decoder::new();
    let mut out = Vec::new();
    for b in &bytes {
        d.push(std::slice::from_ref(b), &mut out);
    }
    assert_eq!(out.len(), 1);
    assert_eq!(d.diagnostics().discarded_bytes, 0);
}

#[test]
fn coalesced_frames_keep_every_pan_and_sequence_wrap() {
    let stream = [
        with_pan(255, b'A'),
        with_pan(0, b'B'),
        with_pan(0, b'Y'),
        with_pan(17, b'Z'),
    ]
    .concat();
    let mut d = Decoder::new();
    let frames = d.decode(&stream);
    let seq: Vec<_> = frames.iter().map(|f| f.header().sequence).collect();
    let pans: Vec<_> = frames.iter().map(|f| f.pan()).collect();
    assert_eq!(seq, [255, 0, 0, 17]);
    assert_eq!(pans, [PanId::A, PanId::B, PanId::Y, PanId::Z]);
    assert_eq!(d.diagnostics().frames_decoded, 4);
}

#[test]
fn huge_garbage_is_bounded_and_keeps_only_sync_prefix() {
    let mut d = Decoder::new();
    let garbage = vec![0xffu8; 1_000_000];
    assert!(d.decode(&garbage).is_empty());
    assert!(d.decode(&[0xff, 0xfe, 0x01]).is_empty());
    assert_eq!(d.pending_bytes(), 3);
    assert_eq!(d.diagnostics().discarded_bytes, 1_000_000);
    assert_eq!(d.decode(&simple(1)[3..]).len(), 1);
    assert_eq!(d.pending_bytes(), 0);
}

#[test]
fn valid_sync_inside_truncated_candidate_is_recovered() {
    let stream = [&simple(1)[..101], &simple(44)[..]].concat();
    let mut d = Decoder::new();
    let frames = d.decode(&stream);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].header().sequence, 44);
    assert_eq!(d.diagnostics().invalid_checksums, 1);
    assert_eq!(d.diagnostics().discarded_bytes, 101);
}

#[test]
fn header_and_payload_corruption_are_distinct_and_recover() {
    let mut bad_header = simple(1);
    bad_header[30] = 1;
    let mut bad_payload = simple(2);
    bad_payload[100] ^= 1;
    let mut d = Decoder::new();
    let frames = d.decode(&[bad_header, bad_payload, simple(99)].concat());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].header().sequence, 99);
    assert_eq!(d.diagnostics().invalid_headers, 1);
    assert_eq!(d.diagnostics().invalid_checksums, 1);
    assert_eq!(d.diagnostics().discarded_bytes, (FRAME_LEN * 2) as u64);
}

#[test]
fn crc_footer_is_big_endian() {
    let mut bytes = simple(1);
    assert_ne!(bytes[4160], bytes[4161]);
    bytes.swap(4160, 4161);
    let mut d = Decoder::new();
    let frames = d.decode(&[bytes, simple(32)].concat());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].header().sequence, 32);
    assert_eq!(d.diagnostics().invalid_checksums, 1);
}

#[test]
fn unknown_versions_and_pans_are_rejected() {
    let stream = [
        packet(1, b'A', 3, "192000", "00014074000", "-1234", &[]),
        packet(1, b'C', 2, "192000", "00014074000", "-1234", &[]),
        with_pan(1, b'B'),
    ]
    .concat();
    let mut d = Decoder::new();
    let frames = d.decode(&stream);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pan(), PanId::B);
    let diag = d.diagnostics();
    assert_eq!(diag.unsupported_versions, 1);
    assert_eq!(diag.unsupported_pans, 1);
    assert_eq!(diag.invalid_headers, 0);
}

#[test]
fn malformed_metadata_never_becomes_a_plausible_value() {
    let malformed = [
        ("000000", "00014074000", "-1234"),
        ("-00001", "00014074000", "-1234"),
        ("12.345", "00014074000", "-1234"),
        (" 12 34", "00014074000", "-1234"),
        ("      ", "00014074000", "-1234"),
        ("\t12000", "00014074000", "-1234"),
        ("12000\n", "00014074000", "-1234"),
        ("192000", "-0000000001", "-1234"),
        ("192000", "00014074e00", "-1234"),
        ("192000", "00014074000", "1.234"),
        ("192000", "00014074000", "-12\x004"),
    ];
    let mut stream: Vec<u8> = malformed
        .iter()
        .flat_map(|(r, c, n)| packet(1, b'A', 2, r, c, n, &[]))
        .collect();
    stream.extend(simple(82));
    let mut d = Decoder::new();
    let frames = d.decode(&stream);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].header().sequence, 82);
    assert_eq!(d.diagnostics().invalid_headers, malformed.len() as u64);
}

#[test]
fn from_bytes_rejects_wrong_length_and_bad_crc() {
    assert!(SpectrumFrame::from_bytes(&[0u8; 10]).is_err());
    let mut b = simple(1);
    b[500] ^= 0x40;
    assert!(matches!(
        SpectrumFrame::from_bytes(&b),
        Err(k4na_protocol::FrameRejection::BadChecksum { .. })
    ));
}

#[test]
fn discard_partial_after_reconnect() {
    let mut d = Decoder::new();
    assert!(d.decode(&simple(1)[..1000]).is_empty());
    d.discard_partial();
    assert_eq!(d.pending_bytes(), 0);
    assert_eq!(d.decode(&simple(2)).len(), 1);
}

proptest! {
    /// Arbitrary chunking of interleaved valid frames and garbage yields
    /// exactly the valid frames, in order, with bounded pending bytes.
    #[test]
    fn random_chunking_and_garbage(
        seqs in proptest::collection::vec(any::<u8>(), 1..8),
        garbage in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 8),
        cuts in proptest::collection::vec(1usize..5000, 1..40),
    ) {
        let mut stream = Vec::new();
        for (i, s) in seqs.iter().enumerate() {
            // Garbage without a sync marker cannot create or hide frames.
            let g: Vec<u8> = garbage[i].iter().map(|b| if *b == 0xff { 0x00 } else { *b }).collect();
            stream.extend(g);
            stream.extend(with_pan(*s, b"ABYZ"[i % 4]));
        }
        let mut d = Decoder::new();
        let mut out = Vec::new();
        let mut offset = 0;
        let mut ci = 0;
        while offset < stream.len() {
            let n = cuts[ci % cuts.len()].min(stream.len() - offset);
            d.push(&stream[offset..offset + n], &mut out);
            prop_assert!(d.pending_bytes() <= FRAME_LEN);
            offset += n;
            ci += 1;
        }
        let got: Vec<u8> = out.iter().map(|f| f.header().sequence).collect();
        prop_assert_eq!(got, seqs);
    }

    /// Random bytes never panic and never exceed one frame of buffering.
    #[test]
    fn arbitrary_bytes_are_safe(data in proptest::collection::vec(any::<u8>(), 0..20000)) {
        let mut d = Decoder::new();
        let frames = d.decode(&data);
        prop_assert!(d.pending_bytes() <= FRAME_LEN);
        for f in frames {
            prop_assert!(SpectrumFrame::from_bytes(f.raw()).is_ok());
        }
    }
}
