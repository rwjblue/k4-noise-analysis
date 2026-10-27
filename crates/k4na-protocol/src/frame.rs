// Copyright (c) 2026 Robert Jackson <me@rwjblue.com>
// SPDX-License-Identifier: MPL-2.0
//
// Ported from Ham2K HaLo `app/lib/services/cat/drivers/k4_spectrum_codec.dart`
// (rwjblue/halo@2df35827261e30721bdd02b86c5e91e1ac7cf1fc). See
// tests/fixtures/k4-spectrum/README.md for provenance and evidence limits.

//! K4 LAN (TCP 9201) version-2 spectrum frame layout.
//!
//! | Offset | Bytes | Meaning |
//! | --- | ---: | --- |
//! | 0 | 4 | Sync `FF FE 01 00` |
//! | 4 | 1 | Version, expected `2` |
//! | 5 | 1 | Sequence byte (per-pan on R39; not a loss counter) |
//! | 6 | 1 | ASCII pan id: `A` main, `B` sub, `Y`/`Z` mini-pans |
//! | 7 | 6 | ASCII sample rate, Hz |
//! | 13 | 11 | ASCII center frequency, Hz |
//! | 24 | 5 | ASCII "noise" header field, tenths of a dB (metadata only) |
//! | 29 | 35 | Uninterpreted header bytes (preserved verbatim) |
//! | 64 | 4096 | 2048 signed big-endian i16 levels, tenths of a dB |
//! | 4160 | 2 | Big-endian CRC-16/IBM-SDLC over bytes 0..4160 |
//!
//! The 64 header bytes sum to zero modulo 256. Levels are native K4 pan
//! units and are **not** established as calibrated dBm.

use std::fmt;

pub const HEADER_LEN: usize = 64;
pub const BIN_COUNT: usize = 2048;
pub const FRAME_LEN: usize = HEADER_LEN + BIN_COUNT * 2 + 2;
pub const PAYLOAD_END: usize = FRAME_LEN - 2;
pub const SUPPORTED_VERSION: u8 = 2;
pub const SYNC: [u8; 4] = [0xff, 0xfe, 0x01, 0x00];

/// Identifies the decoder implementation recorded alongside stored data.
/// Bump when acceptance rules or field interpretation change.
pub const DECODER_VERSION: &str = "k4na-lan-v2-decoder/1";

/// Pan identifier carried in header byte 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum PanId {
    /// Main receiver pan.
    A,
    /// Sub receiver pan.
    B,
    /// Mini-pan (not used for coverage in v1).
    Y,
    /// Mini-pan (not used for coverage in v1).
    Z,
}

impl PanId {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'A' => Some(Self::A),
            b'B' => Some(Self::B),
            b'Y' => Some(Self::Y),
            b'Z' => Some(Self::Z),
            _ => None,
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Self::A => b'A',
            Self::B => b'B',
            Self::Y => b'Y',
            Self::Z => b'Z',
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::Y => "Y",
            Self::Z => "Z",
        }
    }

    pub fn is_mini_pan(self) -> bool {
        matches!(self, Self::Y | Self::Z)
    }
}

impl fmt::Display for PanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PanId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.as_bytes() {
            [b] => PanId::from_byte(*b).ok_or_else(|| format!("unknown pan id {s:?}")),
            _ => Err(format!("unknown pan id {s:?}")),
        }
    }
}

/// Strictly parsed numeric header fields. The raw header bytes stay
/// available through [`SpectrumFrame::header_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrameHeader {
    pub version: u8,
    pub sequence: u8,
    pub pan: PanId,
    pub sample_rate_hz: u32,
    pub center_hz: u64,
    /// Header "noise"/AutoRef field in tenths of a dB. Metadata, not a
    /// measured noise floor.
    pub noise_field_tenths: i32,
}

/// Why a candidate header was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderRejection {
    BadSync,
    BadHeaderSum,
    UnsupportedVersion(u8),
    UnsupportedPan(u8),
    MalformedField(&'static str),
}

/// Parses and validates a 64-byte header candidate.
pub fn parse_header(header: &[u8]) -> Result<FrameHeader, HeaderRejection> {
    if header.len() < HEADER_LEN || header[..4] != SYNC {
        return Err(HeaderRejection::BadSync);
    }
    let sum = header[..HEADER_LEN].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    if sum != 0 {
        return Err(HeaderRejection::BadHeaderSum);
    }
    if header[4] != SUPPORTED_VERSION {
        return Err(HeaderRejection::UnsupportedVersion(header[4]));
    }
    let pan = PanId::from_byte(header[6]).ok_or(HeaderRejection::UnsupportedPan(header[6]))?;
    let rate = parse_ascii_integer(&header[7..13]).ok_or(HeaderRejection::MalformedField("sample_rate"))?;
    let center = parse_ascii_integer(&header[13..24]).ok_or(HeaderRejection::MalformedField("center"))?;
    let noise = parse_ascii_integer(&header[24..29]).ok_or(HeaderRejection::MalformedField("noise"))?;
    if rate <= 0 {
        return Err(HeaderRejection::MalformedField("sample_rate"));
    }
    if center < 0 {
        return Err(HeaderRejection::MalformedField("center"));
    }
    Ok(FrameHeader {
        version: header[4],
        sequence: header[5],
        pan,
        sample_rate_hz: u32::try_from(rate).map_err(|_| HeaderRejection::MalformedField("sample_rate"))?,
        center_hz: u64::try_from(center).map_err(|_| HeaderRejection::MalformedField("center"))?,
        noise_field_tenths: i32::try_from(noise).map_err(|_| HeaderRejection::MalformedField("noise"))?,
    })
}

/// Matches `^ *[+-]?[0-9]+ *$` over ASCII bytes, as the HaLo decoder does.
/// Anything else (embedded spaces, NULs, other characters) is malformed.
fn parse_ascii_integer(field: &[u8]) -> Option<i64> {
    let start = field.iter().position(|b| *b != b' ')?;
    let end = field.iter().rposition(|b| *b != b' ')? + 1;
    let body = &field[start..end];
    let (negative, digits) = match body.first()? {
        b'+' => (false, &body[1..]),
        b'-' => (true, &body[1..]),
        _ => (false, body),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: i64 = 0;
    for d in digits {
        value = value.checked_mul(10)?.checked_add(i64::from(d - b'0'))?;
    }
    Some(if negative { -value } else { value })
}

/// CRC-16/IBM-SDLC (X-25): reflected polynomial 0x8408, init 0xFFFF, final
/// XOR 0xFFFF. The frame footer stores the result most significant byte first.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    crc ^ 0xffff
}

/// One complete, checksum-verified frame. Owns the exact wire bytes so a
/// sampled frame can be stored and replayed through the decoder unchanged.
#[derive(Clone, PartialEq, Eq)]
pub struct SpectrumFrame {
    raw: Box<[u8; FRAME_LEN]>,
    header: FrameHeader,
}

impl fmt::Debug for SpectrumFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpectrumFrame")
            .field("header", &self.header)
            .field("crc", &format_args!("{:04X}", self.crc()))
            .finish()
    }
}

/// Why a complete candidate frame was rejected by [`SpectrumFrame::from_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRejection {
    WrongLength(usize),
    Header(HeaderRejection),
    BadChecksum { expected: u16, actual: u16 },
}

impl fmt::Display for FrameRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength(n) => write!(f, "frame is {n} bytes, expected {FRAME_LEN}"),
            Self::Header(h) => write!(f, "invalid header: {h:?}"),
            Self::BadChecksum { expected, actual } => {
                write!(f, "CRC mismatch: footer {expected:04X}, computed {actual:04X}")
            }
        }
    }
}

impl std::error::Error for FrameRejection {}

impl SpectrumFrame {
    /// Validates a complete frame (header rules and CRC).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FrameRejection> {
        let raw: [u8; FRAME_LEN] = bytes.try_into().map_err(|_| FrameRejection::WrongLength(bytes.len()))?;
        let header = parse_header(&raw[..HEADER_LEN]).map_err(FrameRejection::Header)?;
        let expected = u16::from_be_bytes([raw[PAYLOAD_END], raw[PAYLOAD_END + 1]]);
        let actual = crc16(&raw[..PAYLOAD_END]);
        if expected != actual {
            return Err(FrameRejection::BadChecksum { expected, actual });
        }
        Ok(Self {
            raw: Box::new(raw),
            header,
        })
    }

    pub fn header(&self) -> &FrameHeader {
        &self.header
    }

    pub fn pan(&self) -> PanId {
        self.header.pan
    }

    /// Exact original wire bytes, including header and CRC footer.
    pub fn raw(&self) -> &[u8; FRAME_LEN] {
        &self.raw
    }

    pub fn header_bytes(&self) -> &[u8] {
        &self.raw[..HEADER_LEN]
    }

    pub fn crc(&self) -> u16 {
        u16::from_be_bytes([self.raw[PAYLOAD_END], self.raw[PAYLOAD_END + 1]])
    }

    /// Native signed tenths-of-dB level of bin `i` (zero-based, wire order).
    pub fn bin(&self, i: usize) -> i16 {
        let o = HEADER_LEN + i * 2;
        i16::from_be_bytes([self.raw[o], self.raw[o + 1]])
    }

    /// Copies all bins, in wire order, into `out`.
    pub fn bins_into(&self, out: &mut [i16; BIN_COUNT]) {
        let (pairs, _) = self.raw[HEADER_LEN..PAYLOAD_END].as_chunks::<2>();
        for (slot, pair) in out.iter_mut().zip(pairs) {
            *slot = i16::from_be_bytes(*pair);
        }
    }

    pub fn bins(&self) -> Box<[i16; BIN_COUNT]> {
        let mut out = Box::new([0i16; BIN_COUNT]);
        self.bins_into(&mut out);
        out
    }
}
