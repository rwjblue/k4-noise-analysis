//! K4 `BN` band numbers. Used only to select the radio band before `FA` so
//! band-pass filters and band stacks follow the K4's own band logic.

/// Amateur band edges used to choose a K4 band number (receive-only use;
/// these are not transmit permissions).
const BANDS: &[(u8, u64, u64)] = &[
    (0, 1_800_000, 2_000_000),
    (1, 3_500_000, 4_000_000),
    (2, 5_250_000, 5_450_000),
    (3, 7_000_000, 7_300_000),
    (4, 10_100_000, 10_150_000),
    (5, 14_000_000, 14_350_000),
    (6, 18_068_000, 18_168_000),
    (7, 21_000_000, 21_450_000),
    (8, 24_890_000, 24_990_000),
    (9, 28_000_000, 29_700_000),
    (10, 50_000_000, 54_000_000),
];

/// K4 band number (`BN00`..`BN10`) for a frequency, or `None` outside the
/// amateur bands (general coverage has no fixed band number).
pub fn k4_band_number(hz: u64) -> Option<u8> {
    BANDS
        .iter()
        .find(|(_, lo, hi)| hz >= *lo && hz < *hi)
        .map(|(n, _, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_hf_bands() {
        assert_eq!(k4_band_number(3_500_000), Some(1));
        assert_eq!(k4_band_number(14_175_000), Some(5));
        assert_eq!(k4_band_number(29_699_999), Some(9));
        assert_eq!(k4_band_number(29_700_000), None);
        assert_eq!(k4_band_number(9_000_000), None);
    }
}
