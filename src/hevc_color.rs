//! BT.709 limited-range VUI repair for our VA HEVC Annex B output.
//! This deliberately rejects SPS features our encoder does not emit rather than
//! guessing their bit lengths. Slice data and other NAL units are never rewritten.
use anyhow::{Result, ensure};

/// Return a replacement access unit only when an SPS needs different metadata.
pub fn patch_access_unit(data: &[u8]) -> Result<Option<Vec<u8>>> {
    let starts: Vec<_> = data
        .windows(3)
        .enumerate()
        .filter_map(|(i, w)| (w == [0, 0, 1]).then_some(i + 3))
        .collect();
    ensure!(!starts.is_empty(), "HEVC access unit is not Annex B");
    let mut output = Vec::new();
    let mut copied = 0;
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(data.len(), |next| next - 3);
        ensure!(end >= start + 2, "Truncated HEVC NAL header");
        if (data[start] >> 1) & 63 != 33 {
            continue;
        }
        ensure!(
            data[start] & 0x81 == 0 && data[start + 1] == 1,
            "Unsupported HEVC SPS layer or temporal identifier"
        );
        // Zero bytes before the next start code belong to Annex B, not the RBSP.
        let mut payload_end = end;
        while payload_end > start + 2 && data[payload_end - 1] == 0 {
            payload_end -= 1;
        }
        if let Some(sps) = patch_sps(&data[start + 2..payload_end])? {
            output.extend_from_slice(&data[copied..start + 2]);
            output.extend_from_slice(&sps);
            copied = payload_end;
        }
    }
    if copied == 0 {
        return Ok(None);
    }
    output.extend_from_slice(&data[copied..]);
    Ok(Some(output))
}

struct Bits {
    bits: Vec<bool>,
    pos: usize,
}
impl Bits {
    fn take(&mut self, n: usize) -> Result<u32> {
        ensure!(
            n <= 32 && self.pos + n <= self.bits.len(),
            "Truncated HEVC SPS"
        );
        let mut v = 0;
        for &bit in &self.bits[self.pos..self.pos + n] {
            v = (v << 1) | u32::from(bit);
        }
        self.pos += n;
        Ok(v)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.take(1)? != 0)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        ensure!(self.pos + n <= self.bits.len(), "Truncated HEVC SPS");
        self.pos += n;
        Ok(())
    }
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            ensure!(zeros <= 31, "Invalid SPS Exp-Golomb value");
        }
        Ok(((1u32 << zeros) - 1) + self.take(zeros)?)
    }
    fn ues(&mut self, n: usize) -> Result<()> {
        for _ in 0..n {
            self.ue()?;
        }
        Ok(())
    }
}

fn unescape(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !data.is_empty() && data.len() <= 4096,
        "Invalid HEVC SPS size"
    );
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for (i, &byte) in data.iter().enumerate() {
        if zeros == 2 && byte == 3 {
            ensure!(
                data.get(i + 1).is_some_and(|&b| b <= 3),
                "Invalid SPS emulation prevention"
            );
            zeros = 0;
            continue;
        }
        ensure!(
            zeros < 2 || byte > 2,
            "Unescaped HEVC SPS start-code pattern"
        );
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    Ok(out)
}
fn escape(bits: &[bool]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut zeros = 0;
    for chunk in bits.chunks(8) {
        let mut byte = 0;
        for i in 0..8 {
            byte = (byte << 1) | u8::from(chunk.get(i).copied().unwrap_or(false));
        }
        if zeros == 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}
fn color_bits() -> Vec<bool> {
    // signal present, unspecified video format (5), limited, description present,
    // BT.709 primaries, transfer characteristics and matrix coefficients.
    let mut bits = vec![true, true, false, true, false, true];
    for _ in 0..3 {
        bits.extend([false, false, false, false, false, false, false, true]);
    }
    bits
}
fn patch_sps(data: &[u8]) -> Result<Option<Vec<u8>>> {
    let rbsp = unescape(data)?;
    let bits = rbsp
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| b & (1 << i) != 0))
        .collect();
    let mut r = Bits { bits, pos: 0 };
    r.skip(4)?;
    let sublayers = r.take(3)? as usize;
    ensure!(sublayers <= 6, "Invalid HEVC sublayer count");
    r.skip(1 + 96)?; // nesting and general profile_tier_level
    let mut layers = Vec::new();
    for _ in 0..sublayers {
        layers.push((r.flag()?, r.flag()?));
    }
    if sublayers > 0 {
        r.skip((8 - sublayers) * 2)?;
    }
    for (profile, level) in layers {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    r.ue()?; // SPS id
    let chroma = r.ue()?;
    ensure!(chroma <= 3, "Invalid HEVC chroma format");
    if chroma == 3 {
        r.skip(1)?;
    }
    r.ues(2)?; // width and height
    if r.flag()? {
        r.ues(4)?;
    }
    r.ues(2)?; // bit depth
    let poc = r.ue()?;
    ensure!(poc <= 12, "Invalid HEVC POC width");
    let ordering_all = r.flag()?;
    r.ues(if ordering_all { (sublayers + 1) * 3 } else { 3 })?;
    r.ues(6)?; // block/transform sizes and hierarchy depths
    if r.flag()? {
        ensure!(!r.flag()?, "Unsupported SPS scaling-list data");
    }
    r.skip(2)?; // AMP and SAO
    ensure!(!r.flag()?, "Unsupported SPS PCM");
    ensure!(r.ue()? == 0, "Unsupported SPS short-term reference sets");
    if r.flag()? {
        let count = r.ue()? as usize;
        ensure!(count <= 32, "Invalid SPS long-term reference count");
        r.skip(count * (poc as usize + 5))?;
    }
    r.skip(2)?; // temporal MVP and intra smoothing
    let vui_start = r.pos;
    let vui = r.flag()?;
    let (start, end, replacement) = if vui {
        if r.flag()? && r.take(8)? == 255 {
            r.skip(32)?;
        }
        if r.flag()? {
            r.skip(1)?;
        }
        let start = r.pos;
        if r.flag()? {
            r.skip(4)?;
            if r.flag()? {
                r.skip(24)?;
            }
        }
        let end = r.pos;
        if r.flag()? {
            r.ues(2)?;
        } // chroma location
        r.skip(3)?;
        if r.flag()? {
            r.ues(4)?;
        }
        if r.flag()? {
            r.skip(64)?;
            if r.flag()? {
                r.ue()?;
            }
            ensure!(!r.flag()?, "Unsupported SPS HRD parameters");
        }
        if r.flag()? {
            r.skip(3)?;
            r.ues(5)?;
        }
        (start, end, color_bits())
    } else {
        let mut replacement = vec![true, false, false]; // VUI, no SAR/overscan
        replacement.extend(color_bits());
        replacement.extend([false; 7]); // chroma location through bitstream restriction
        (vui_start, r.pos, replacement)
    };
    ensure!(!r.flag()?, "Unsupported SPS extension");
    ensure!(r.flag()?, "Missing SPS RBSP stop bit");
    let content_end = r.pos;
    ensure!(
        r.bits.len() - content_end < 8 && r.bits[content_end..].iter().all(|b| !b),
        "Invalid SPS RBSP trailing bits"
    );
    if r.bits[start..end] == replacement {
        return Ok(None);
    }
    let mut patched = r.bits[..start].to_vec();
    patched.extend(replacement);
    patched.extend_from_slice(&r.bits[end..content_end]);
    Ok(Some(escape(&patched)))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Synthetic videotestsrc VA HEVC Main SPS: 1920x1080, SAR 1:1, 60fps.
    const SPS_HEX: &str = "42010101600000030090000003000003007ba003c0801107cbb2e491b6affc0004000404000003000400000300f020";
    fn fixture() -> Vec<u8> {
        let mut s = vec![0, 0, 0, 1];
        s.extend(
            (0..SPS_HEX.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&SPS_HEX[i..i + 2], 16).unwrap()),
        );
        s
    }
    #[test]
    fn repairs_real_sps_preserves_other_nals_and_is_idempotent() {
        let mut au = vec![0, 0, 1, 0x46, 1, 0x50];
        au.extend(fixture());
        let tail = [0, 0, 0, 1, 0x26, 1, 0xab, 0x80, 0, 0];
        au.extend(tail);
        let patched = patch_access_unit(&au).unwrap().unwrap();
        assert_eq!(&patched[..6], &au[..6]);
        assert!(patched.ends_with(&tail));
        assert!(patch_access_unit(&patched).unwrap().is_none());
        let original = unescape(&fixture()[6..]).unwrap();
        let patched_rbsp = unescape(&patched[12..patched.len() - tail.len()]).unwrap();
        let before: Vec<_> = original
            .iter()
            .flat_map(|b| (0..8).rev().map(move |i| b & (1 << i) != 0))
            .collect();
        let after: Vec<_> = patched_rbsp
            .iter()
            .flat_map(|b| (0..8).rev().map(move |i| b & (1 << i) != 0))
            .collect();
        assert_eq!(&before[..239], &after[..239]);
        assert_eq!(&after[239..269], color_bits());
        assert_eq!(&before[240..315], &after[269..344]);
    }
    #[test]
    fn rejects_truncated_sps_and_bad_emulation_prevention() {
        let s = fixture();
        for len in 6..s.len() {
            assert!(patch_access_unit(&s[..len]).is_err(), "length {len}");
        }
        assert!(unescape(&[0, 0, 3]).is_err());
        assert!(unescape(&[0, 0, 3, 4]).is_err());
        assert!(unescape(&[0, 0, 2]).is_err());
        assert!(patch_access_unit(&[0, 0, 1, 0x42]).is_err());
    }
    #[test]
    fn leaves_non_sps_units_byte_identical() {
        assert!(
            patch_access_unit(&[0, 0, 1, 0x26, 1, 0x80])
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn handles_absent_vui_and_existing_incorrect_color_fields() {
        let original = fixture();
        let rbsp = unescape(&original[6..]).unwrap();
        let bits: Vec<_> = rbsp
            .iter()
            .flat_map(|b| (0..8).rev().map(move |i| b & (1 << i) != 0))
            .collect();
        // Original VUI starts at bit 196, SPS extension at 313, stop at 314.
        let mut no_vui = bits[..196].to_vec();
        no_vui.push(false);
        no_vui.extend_from_slice(&bits[313..315]);
        let mut au = original[..6].to_vec();
        au.extend(escape(&no_vui));
        let patched = patch_access_unit(&au).unwrap().unwrap();
        assert!(patch_access_unit(&patched).unwrap().is_none());
        for color in [vec![true, true, false, true, true, false], vec![true; 30]] {
            let mut wrong = bits[..239].to_vec();
            wrong.extend(color);
            wrong.extend_from_slice(&bits[240..315]);
            let mut au = original[..6].to_vec();
            au.extend(escape(&wrong));
            assert_eq!(
                patch_access_unit(&au).unwrap(),
                patch_access_unit(&original).unwrap()
            );
        }
    }

    #[test]
    fn escape_roundtrip_including_emulated_start_codes() {
        let bytes = [0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 3, 0x80];
        let bits: Vec<_> = bytes
            .iter()
            .flat_map(|b| (0..8).rev().map(move |i| b & (1 << i) != 0))
            .collect();
        assert_eq!(unescape(&escape(&bits)).unwrap(), bytes);
    }
}
