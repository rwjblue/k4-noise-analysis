// SPDX-License-Identifier: MPL-2.0

//! Receive-only K4 LAN spectrum (TCP 9201) protocol: framing, header and CRC
//! validation. This crate has no radio control, networking or storage.

mod decoder;
mod frame;
pub mod synth;

pub use decoder::{Decoder, DecoderDiagnostics};
pub use frame::{
    BIN_COUNT, DECODER_VERSION, FRAME_LEN, FrameHeader, FrameRejection, HEADER_LEN, HeaderRejection, PAYLOAD_END,
    PanId, SUPPORTED_VERSION, SYNC, SpectrumFrame, crc16, parse_header,
};
