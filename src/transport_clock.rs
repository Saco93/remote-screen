use anyhow::{Result, ensure};

const TS_PACKET_SIZE: usize = 188;
const PCR_MODULUS: u64 = (1u64 << 33) * 300;

/// Advance transport clock references without changing PES timestamps or payload.
/// Validate the entire batch before changing bytes, so malformed input is atomic.
pub fn advance_pcr(data: &mut [u8], advance_ms: u32) -> Result<usize> {
    ensure!(
        advance_ms <= 125,
        "PCR advance exceeds the mux's 125 ms offset"
    );
    ensure!(
        data.len().is_multiple_of(TS_PACKET_SIZE),
        "Truncated TS packet"
    );
    for packet in data.as_chunks::<TS_PACKET_SIZE>().0 {
        pcr_offset(packet)?;
    }
    if advance_ms == 0 {
        return Ok(0);
    }
    let mut changed = 0;
    for packet in data.as_chunks_mut::<TS_PACKET_SIZE>().0 {
        if let Some(offset) = pcr_offset(packet)? {
            let pcr = &mut packet[offset..offset + 6];
            let base = (u64::from(pcr[0]) << 25)
                | (u64::from(pcr[1]) << 17)
                | (u64::from(pcr[2]) << 9)
                | (u64::from(pcr[3]) << 1)
                | u64::from(pcr[4] >> 7);
            let extension = (u64::from(pcr[4] & 1) << 8) | u64::from(pcr[5]);
            let shifted = (base * 300 + extension + u64::from(advance_ms) * 27_000) % PCR_MODULUS;
            let base = shifted / 300;
            let extension = shifted % 300;
            pcr[0] = (base >> 25) as u8;
            pcr[1] = (base >> 17) as u8;
            pcr[2] = (base >> 9) as u8;
            pcr[3] = (base >> 1) as u8;
            pcr[4] = ((base as u8 & 1) << 7) | (pcr[4] & 0x7e) | ((extension >> 8) as u8 & 1);
            pcr[5] = extension as u8;
            changed += 1;
        }
    }
    Ok(changed)
}

fn pcr_offset(packet: &[u8]) -> Result<Option<usize>> {
    ensure!(packet[0] == 0x47, "Invalid TS sync byte");
    let adaptation = (packet[3] >> 4) & 3;
    ensure!(adaptation != 0, "Reserved TS adaptation field control");
    if adaptation & 2 == 0 {
        return Ok(None);
    }
    let length = usize::from(packet[4]);
    ensure!(length <= 183, "TS adaptation field exceeds packet");
    if length == 0 {
        return Ok(None);
    }
    let flags = packet[5];
    if flags & 0x10 == 0 {
        return Ok(None);
    }
    ensure!(length >= 7, "Truncated TS PCR field");
    // OPCR is a separate original-clock reference. Keep its value intact, but
    // reject a flag whose six bytes do not fit in this adaptation field.
    ensure!(flags & 0x08 == 0 || length >= 13, "Truncated TS OPCR field");
    let extension = (u16::from(packet[10] & 1) << 8) | u16::from(packet[11]);
    ensure!(extension < 300, "Invalid PCR extension");
    Ok(Some(6))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(base: u64, extension: u16, pid: u16) -> Vec<u8> {
        let mut packet = vec![0x55; TS_PACKET_SIZE];
        packet[..6].copy_from_slice(&[
            0x47,
            ((pid >> 8) as u8 & 31) | 0x40,
            pid as u8,
            0x30,
            7,
            0x10,
        ]);
        packet[6..12].copy_from_slice(&[
            (base >> 25) as u8,
            (base >> 17) as u8,
            (base >> 9) as u8,
            (base >> 1) as u8,
            ((base as u8 & 1) << 7) | 0x2a | ((extension >> 8) as u8),
            extension as u8,
        ]);
        packet
    }

    #[test]
    fn zero_advance_keeps_every_byte() {
        let mut bytes = packet(100, 299, 4113);
        let original = bytes.clone();
        assert_eq!(advance_pcr(&mut bytes, 0).unwrap(), 0);
        assert_eq!(bytes, original);
    }

    #[test]
    fn advances_all_pids_preserving_payload_reserved_bits_and_extension() {
        let mut bytes = packet(90000, 257, 4113);
        bytes.extend(packet(180000, 299, 4352));
        let original = bytes.clone();
        assert_eq!(advance_pcr(&mut bytes, 75).unwrap(), 2);
        let mut expected = packet(96750, 257, 4113);
        expected.extend(packet(186750, 299, 4352));
        assert_eq!(bytes, expected);
        for (before, after) in original
            .as_chunks::<188>()
            .0
            .iter()
            .zip(bytes.as_chunks::<188>().0)
        {
            assert_eq!(&before[..6], &after[..6]);
            assert_eq!(&before[12..], &after[12..]);
        }
    }

    #[test]
    fn wraps_full_pcr_clock_preserving_fractional_extension() {
        let mut bytes = packet((1u64 << 33) - 100, 299, 4352);
        advance_pcr(&mut bytes, 75).unwrap();
        assert_eq!(bytes, packet(6650, 299, 4352));
    }

    #[test]
    fn malformed_batch_does_not_partially_modify_previous_packets() {
        for (length, extension) in [(6, 0), (184, 0), (7, 300)] {
            let mut bytes = packet(100, 0, 4113);
            let mut invalid = packet(200, extension, 4352);
            invalid[4] = length;
            bytes.extend(invalid);
            let original = bytes.clone();
            assert!(advance_pcr(&mut bytes, 75).is_err());
            assert_eq!(bytes, original);
        }
        assert!(advance_pcr(&mut [0x47; 187], 75).is_err());
        assert!(advance_pcr(&mut packet(0, 0, 4113), 126).is_err());
    }

    #[test]
    fn leaves_original_clock_reference_intact() {
        let mut bytes = packet(100, 0, 4113);
        bytes[4] = 13;
        bytes[5] |= 0x08;
        let original = bytes.clone();
        advance_pcr(&mut bytes, 75).unwrap();
        assert_eq!(&bytes[12..], &original[12..]);
    }
}
