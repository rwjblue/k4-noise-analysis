// Copyright (c) 2026 Robert Jackson <me@rwjblue.com>
// SPDX-License-Identifier: MPL-2.0
//
// Ported from Ham2K HaLo `K4SpectrumDecoder`
// (rwjblue/halo@2df35827261e30721bdd02b86c5e91e1ac7cf1fc).

//! Incremental, bounded framing for the K4 LAN spectrum byte stream.

use crate::frame::{FRAME_LEN, HEADER_LEN, HeaderRejection, PAYLOAD_END, SYNC, SpectrumFrame, crc16, parse_header};

/// Framing results since the decoder was created or reset. Rejections count
/// candidate headers, not presumed lost radio sweeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DecoderDiagnostics {
    pub frames_decoded: u64,
    pub bytes_received: u64,
    pub discarded_bytes: u64,
    pub invalid_headers: u64,
    pub invalid_checksums: u64,
    pub unsupported_versions: u64,
    pub unsupported_pans: u64,
}

impl DecoderDiagnostics {
    pub fn rejected_candidates(&self) -> u64 {
        self.invalid_headers + self.invalid_checksums + self.unsupported_versions + self.unsupported_pans
    }

    /// Element-wise difference `self - earlier`, for per-interval accounting.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            frames_decoded: self.frames_decoded - earlier.frames_decoded,
            bytes_received: self.bytes_received - earlier.bytes_received,
            discarded_bytes: self.discarded_bytes - earlier.discarded_bytes,
            invalid_headers: self.invalid_headers - earlier.invalid_headers,
            invalid_checksums: self.invalid_checksums - earlier.invalid_checksums,
            unsupported_versions: self.unsupported_versions - earlier.unsupported_versions,
            unsupported_pans: self.unsupported_pans - earlier.unsupported_pans,
        }
    }
}

/// Incremental decoder. The retained buffer is exactly one frame regardless
/// of input chunk size, so memory is bounded even for unbounded garbage.
///
/// A bad candidate advances only one byte before searching again: a real
/// frame's sync can occur inside the bytes a damaged header claimed as its
/// payload.
#[derive(Debug)]
pub struct Decoder {
    buffer: Box<[u8; FRAME_LEN]>,
    pending: usize,
    header_ok: bool,
    diagnostics: DecoderDiagnostics,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            buffer: Box::new([0; FRAME_LEN]),
            pending: 0,
            header_ok: false,
            diagnostics: DecoderDiagnostics::default(),
        }
    }

    /// Bytes held for an incomplete candidate frame.
    pub fn pending_bytes(&self) -> usize {
        self.pending
    }

    pub fn diagnostics(&self) -> DecoderDiagnostics {
        self.diagnostics
    }

    /// Discards any partial frame (e.g. after a reconnect) without resetting
    /// diagnostics. The discarded bytes are counted.
    pub fn discard_partial(&mut self) {
        let n = self.pending;
        self.discard(n);
    }

    /// Feeds bytes and appends complete valid frames to `out`, in arrival
    /// order. Sequence bytes are preserved, including repeats and wraparound.
    pub fn push(&mut self, mut chunk: &[u8], out: &mut Vec<SpectrumFrame>) {
        self.diagnostics.bytes_received += chunk.len() as u64;
        while !chunk.is_empty() {
            let take = chunk.len().min(FRAME_LEN - self.pending);
            self.buffer[self.pending..self.pending + take].copy_from_slice(&chunk[..take]);
            self.pending += take;
            chunk = &chunk[take..];
            self.drain(out);
        }
    }

    /// Convenience wrapper returning the frames decoded from `chunk`.
    pub fn decode(&mut self, chunk: &[u8]) -> Vec<SpectrumFrame> {
        let mut out = Vec::new();
        self.push(chunk, &mut out);
        out
    }

    fn drain(&mut self, out: &mut Vec<SpectrumFrame>) {
        while self.pending > 0 {
            if !self.header_ok {
                match self.find_sync() {
                    None => {
                        // Only a suffix matching a prefix of the sync can be useful.
                        let mut keep = self.pending.min(SYNC.len() - 1);
                        while keep > 0 && !self.matches_sync(self.pending - keep, keep) {
                            keep -= 1;
                        }
                        self.discard(self.pending - keep);
                        return;
                    }
                    Some(start) if start > 0 => self.discard(start),
                    Some(_) => {}
                }
                if self.pending < HEADER_LEN {
                    return;
                }
                match parse_header(&self.buffer[..HEADER_LEN]) {
                    Ok(_) => self.header_ok = true,
                    Err(rejection) => {
                        match rejection {
                            HeaderRejection::UnsupportedVersion(_) => self.diagnostics.unsupported_versions += 1,
                            HeaderRejection::UnsupportedPan(_) => self.diagnostics.unsupported_pans += 1,
                            HeaderRejection::BadSync
                            | HeaderRejection::BadHeaderSum
                            | HeaderRejection::MalformedField(_) => {
                                self.diagnostics.invalid_headers += 1;
                            }
                        }
                        self.discard(1);
                        continue;
                    }
                }
            }
            if self.pending < FRAME_LEN {
                return;
            }
            let footer = u16::from_be_bytes([self.buffer[PAYLOAD_END], self.buffer[PAYLOAD_END + 1]]);
            if crc16(&self.buffer[..PAYLOAD_END]) != footer {
                self.diagnostics.invalid_checksums += 1;
                self.discard(1);
                continue;
            }
            match SpectrumFrame::from_bytes(&self.buffer[..]) {
                Ok(frame) => {
                    out.push(frame);
                    self.diagnostics.frames_decoded += 1;
                    self.pending = 0;
                    self.header_ok = false;
                }
                Err(_) => {
                    // Unreachable in practice: header and CRC were checked above.
                    self.diagnostics.invalid_headers += 1;
                    self.discard(1);
                }
            }
        }
    }

    fn find_sync(&self) -> Option<usize> {
        if self.pending < SYNC.len() {
            return None;
        }
        (0..=self.pending - SYNC.len()).find(|&i| self.matches_sync(i, SYNC.len()))
    }

    fn matches_sync(&self, offset: usize, len: usize) -> bool {
        self.buffer[offset..offset + len] == SYNC[..len]
    }

    fn discard(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.buffer.copy_within(count..self.pending, 0);
        self.pending -= count;
        self.diagnostics.discarded_bytes += count as u64;
        self.header_ok = false;
    }
}
