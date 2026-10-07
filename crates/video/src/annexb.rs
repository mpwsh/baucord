//! H.264 Annex-B access units → what `VideoToolbox` takes (IOS-SPEC.md
//! D9, §14 D30).
//!
//! The senders' encoders emit Annex-B: NAL units separated by start
//! codes (`00 00 01` or `00 00 00 01`), with the SPS and PPS in-band
//! before every IDR. `VideoToolbox` wants the other framing, AVCC: the
//! parameter sets go into a format description, and the sample holds
//! each remaining NAL unit behind its length as a 4-byte big-endian
//! number. [`split`] does that conversion for one access unit.
//!
//! Pure code, no platform dependencies: the unit tests run on the host.

/// NAL unit types (`nal_unit_type`, the low five bits of the header).
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
/// Access unit delimiter: framing only, meaningless in AVCC.
pub const NAL_AUD: u8 = 9;

/// The length prefix [`split`] writes, as the format description must
/// be told (`NALUnitHeaderLength`).
pub const AVCC_LENGTH_BYTES: usize = 4;

/// One access unit, re-framed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Split<'a> {
    /// The sequence parameter sets it carried (usually one, before an
    /// IDR; none on a P-frame).
    pub sps: Vec<&'a [u8]>,
    /// The picture parameter sets.
    pub pps: Vec<&'a [u8]>,
    /// Every other NAL unit (slices, SEI), each behind a 4-byte
    /// big-endian length — the sample's bytes. Empty when the unit held
    /// only parameter sets.
    pub avcc: Vec<u8>,
}

/// The NAL units of an Annex-B buffer, without their start codes and
/// without the zero bytes that may pad a unit before the next start
/// code (a NAL unit never ends in `00`: its last byte holds the RBSP
/// stop bit, and trailing `cabac_zero_word`s are emulation-protected).
/// Bytes before the first start code are skipped.
#[must_use]
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                push_trimmed(&mut units, &data[s..i]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        push_trimmed(&mut units, &data[s..]);
    }
    units
}

fn push_trimmed<'a>(units: &mut Vec<&'a [u8]>, unit: &'a [u8]) {
    let end = unit.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    if end > 0 {
        units.push(&unit[..end]);
    }
}

/// The NAL unit type of a unit (its first byte's low five bits).
#[must_use]
pub fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |h| h & 0x1F)
}

/// Re-frame one Annex-B access unit: parameter sets out, the rest
/// length-prefixed into [`Split::avcc`] (reusing `out`'s allocation).
/// Access unit delimiters are dropped.
#[must_use]
pub fn split(data: &[u8], mut out: Vec<u8>) -> Split<'_> {
    out.clear();
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for unit in nal_units(data) {
        match nal_type(unit) {
            NAL_SPS => sps.push(unit),
            NAL_PPS => pps.push(unit),
            NAL_AUD => {}
            _ => {
                // A NAL unit is far below 4 GiB (frames are capped at a
                // few hundred KiB by the transport).
                let len = u32::try_from(unit.len()).unwrap_or(u32::MAX);
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(unit);
            }
        }
    }
    Split {
        sps,
        pps,
        avcc: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 4] = [0x67, 0x42, 0xC0, 0x1F];
    const PPS: [u8; 3] = [0x68, 0xCE, 0x3C];
    const IDR: [u8; 5] = [0x65, 0x88, 0x84, 0x00, 0x33];
    const SLICE: [u8; 3] = [0x41, 0x9A, 0x02];

    fn annexb(units: &[(&[u8], bool)]) -> Vec<u8> {
        let mut v = Vec::new();
        for (unit, long) in units {
            if *long {
                v.push(0);
            }
            v.extend_from_slice(&[0, 0, 1]);
            v.extend_from_slice(unit);
        }
        v
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = annexb(&[(&SPS, true), (&PPS, false), (&IDR, true)]);
        assert_eq!(nal_units(&data), vec![&SPS[..], &PPS[..], &IDR[..]]);
    }

    #[test]
    fn keeps_a_zero_inside_a_unit_and_trims_padding() {
        // IDR has a 00 in the middle; the padding before the next
        // start code (and at the end) is not part of any unit.
        let mut data = annexb(&[(&IDR, false)]);
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&annexb(&[(&SLICE, true)]));
        data.extend_from_slice(&[0, 0, 0]);
        assert_eq!(nal_units(&data), vec![&IDR[..], &SLICE[..]]);
    }

    #[test]
    fn ignores_leading_garbage_and_empty_input() {
        assert!(nal_units(&[]).is_empty());
        assert!(nal_units(&[0x12, 0x34]).is_empty());
        let mut data = vec![0xFF, 0x00];
        data.extend_from_slice(&annexb(&[(&SLICE, false)]));
        assert_eq!(nal_units(&data), vec![&SLICE[..]]);
    }

    #[test]
    fn keyframe_becomes_parameter_sets_and_length_prefixed_slices() {
        let aud: [u8; 2] = [0x09, 0xF0];
        let data = annexb(&[(&aud, true), (&SPS, true), (&PPS, true), (&IDR, true)]);
        let s = split(&data, Vec::new());
        assert_eq!(s.sps, vec![&SPS[..]]);
        assert_eq!(s.pps, vec![&PPS[..]]);
        let mut want = vec![0, 0, 0, 5];
        want.extend_from_slice(&IDR);
        assert_eq!(s.avcc, want);
    }

    #[test]
    fn p_frame_has_no_parameter_sets() {
        let data = annexb(&[(&SLICE, true), (&SLICE, false)]);
        let s = split(&data, vec![9, 9, 9]);
        assert!(s.sps.is_empty() && s.pps.is_empty());
        assert_eq!(
            s.avcc,
            [0, 0, 0, 3, 0x41, 0x9A, 0x02, 0, 0, 0, 3, 0x41, 0x9A, 0x02]
        );
    }

    #[test]
    fn nal_type_reads_the_low_five_bits() {
        assert_eq!(nal_type(&SPS), NAL_SPS);
        assert_eq!(nal_type(&PPS), NAL_PPS);
        assert_eq!(nal_type(&IDR), 5);
        assert_eq!(nal_type(&[]), 0);
    }
}
