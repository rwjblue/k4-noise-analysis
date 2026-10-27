// SPDX-License-Identifier: MPL-2.0

//! Builder for **synthetic** frames used by tests and the K4 simulator.
//!
//! Frames produced here are protocol-shaped but are not evidence about real
//! firmware behavior; only the hardware fixtures can validate the format.

use crate::frame::{FRAME_LEN, HEADER_LEN, PAYLOAD_END, PanId, SUPPORTED_VERSION, SYNC, crc16};

/// Parameters for a synthetic frame.
#[derive(Debug, Clone)]
pub struct SyntheticFrame {
    pub version: u8,
    pub sequence: u8,
    pub pan: PanId,
    pub sample_rate_hz: u32,
    pub center_hz: u64,
    pub noise_field_tenths: i32,
}

impl Default for SyntheticFrame {
    fn default() -> Self {
        Self {
            version: SUPPORTED_VERSION,
            sequence: 0,
            pan: PanId::A,
            sample_rate_hz: 96_000,
            center_hz: 14_045_000,
            noise_field_tenths: -1300,
        }
    }
}

impl SyntheticFrame {
    /// Encodes the frame with the given native tenth-dB bins. Bins beyond
    /// `bins.len()` are filled with `-1200`.
    pub fn encode(&self, bins: &[i16]) -> [u8; FRAME_LEN] {
        let mut out = [0u8; FRAME_LEN];
        out[..4].copy_from_slice(&SYNC);
        out[4] = self.version;
        out[5] = self.sequence;
        out[6] = self.pan.as_byte();
        write_field(&mut out[7..13], i64::from(self.sample_rate_hz));
        write_field(&mut out[13..24], i64::try_from(self.center_hz).unwrap_or(i64::MAX));
        write_field(&mut out[24..29], i64::from(self.noise_field_tenths));
        fix_header_sum(&mut out);
        for i in 0..crate::frame::BIN_COUNT {
            let v = bins.get(i).copied().unwrap_or(-1200);
            out[HEADER_LEN + 2 * i..HEADER_LEN + 2 * i + 2].copy_from_slice(&v.to_be_bytes());
        }
        fix_crc(&mut out);
        out
    }
}

/// Writes a zero-padded (sign-preserving) decimal field, as observed on R39.
fn write_field(dst: &mut [u8], value: i64) {
    let width = dst.len();
    let text = if value < 0 {
        format!("-{:0>w$}", -value, w = width - 1)
    } else {
        format!("{value:0>width$}")
    };
    let bytes = text.as_bytes();
    let n = bytes.len().min(width);
    dst[..n].copy_from_slice(&bytes[bytes.len() - n..]);
}

/// Sets header byte 63 so the 64 header bytes sum to zero modulo 256.
pub fn fix_header_sum(frame: &mut [u8]) {
    let sum = frame[..HEADER_LEN - 1].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    frame[HEADER_LEN - 1] = 0u8.wrapping_sub(sum);
}

/// Rewrites the CRC footer to match the payload.
pub fn fix_crc(frame: &mut [u8]) {
    let crc = crc16(&frame[..PAYLOAD_END]);
    frame[PAYLOAD_END..PAYLOAD_END + 2].copy_from_slice(&crc.to_be_bytes());
}
