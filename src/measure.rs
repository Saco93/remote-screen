//! Observe outgoing WFD video timestamps without retaining packet payloads.
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const DESTINATION: [u8; 4] = [192, 168, 49, 1];
const DESTINATION_PORT: u16 = 53000;
const VIDEO_PID: u16 = 4113;
const PTS_MASK: u64 = (1 << 33) - 1;

pub fn run(interface: &str, seconds: u64, stop: &AtomicBool) -> Result<()> {
    ensure!(
        (1..=60).contains(&seconds),
        "Measurement duration must be 1..60 seconds"
    );
    let name = CString::new(interface).context("Invalid network interface name")?;
    // The CString is NUL terminated and remains alive for this call.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    ensure!(
        index != 0,
        "Cannot find interface {interface}: {}",
        io::Error::last_os_error()
    );
    // SOCK_NONBLOCK guarantees recvfrom cannot extend the measurement deadline.
    let raw = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            (libc::ETH_P_ALL as u16).to_be() as i32,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error())
            .context("Open packet socket (requires root or CAP_NET_RAW)");
    }
    // socket returned a new descriptor; OwnedFd is its sole owner on all paths.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    // sockaddr_ll is a C value with no references; zero initializes unused fields.
    let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    address.sll_family = libc::AF_PACKET as u16;
    address.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
    address.sll_ifindex = index as i32;
    // address has the correct family and length for an AF_PACKET bind.
    let result = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_ll).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error())
            .context("Bind packet capture to requested interface");
    }
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut stats = Statistics::default();
    let mut packet = [0u8; 65536];
    crate::status(&format!(
        "Measuring outgoing video on {interface} for {seconds}s (192.168.49.1:53000, PID 4113)"
    ));
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        let mut pollfd = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(100) as i32;
        // pollfd points to one initialized descriptor and timeout is bounded.
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Wait for outgoing RTP packet");
        }
        if ready == 0 {
            continue;
        }
        ensure!(
            pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0,
            "Packet socket became unavailable"
        );
        let mut sender: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of_val(&sender) as libc::socklen_t;
        // packet and sender are writable buffers of the specified lengths.
        let count = unsafe {
            libc::recvfrom(
                socket.as_raw_fd(),
                packet.as_mut_ptr().cast(),
                packet.len(),
                0,
                (&mut sender as *mut libc::sockaddr_ll).cast(),
                &mut length,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(error).context("Read outgoing RTP packet");
        }
        if sender.sll_pkttype != libc::PACKET_OUTGOING || sender.sll_ifindex != index as i32 {
            continue;
        }
        let arrival = started.elapsed().as_secs_f64();
        video_pts(&packet[..count as usize], |pts| stats.record(pts, arrival));
    }
    if stats.seen.len() < 2 && stop.load(Ordering::Relaxed) {
        crate::status("Frame measurement cancelled.");
        return Ok(());
    }
    crate::status(&stats.summary()?);
    crate::status("This measures sender packet output, not TV decoding or presentation.");
    Ok(())
}

#[derive(Default)]
struct Statistics {
    seen: HashSet<u64>,
    first: Option<(u64, f64)>,
    last: Option<(u64, f64)>,
}
impl Statistics {
    fn record(&mut self, pts: u64, arrival: f64) {
        if self.seen.insert(pts) {
            self.first.get_or_insert((pts, arrival));
            self.last = Some((pts, arrival));
        }
    }
    fn summary(&self) -> Result<String> {
        ensure!(
            self.seen.len() >= 2,
            "Not enough outgoing video PES timestamps; check interface and negotiated UDP port"
        );
        let (first_pts, first_arrival) = self.first.context("Missing first frame")?;
        let (last_pts, last_arrival) = self.last.context("Missing last frame")?;
        let wall = last_arrival - first_arrival;
        let span = last_pts.wrapping_sub(first_pts) & PTS_MASK;
        ensure!(
            wall > 0.0 && span > 0,
            "Not enough elapsed time between video frames"
        );
        let intervals = (self.seen.len() - 1) as f64;
        Ok(format!(
            "Outgoing video: {} unique frames; wall interval={wall:.3}s; wall-fps={:.2}; timestamp-fps={:.2}",
            self.seen.len(),
            intervals / wall,
            intervals * 90000.0 / span as f64
        ))
    }
}

fn video_pts(frame: &[u8], mut emit: impl FnMut(u64)) {
    if frame.len() < 42 || frame[12..14] != [0x08, 0x00] {
        return;
    }
    let ip = &frame[14..];
    let header = ((ip[0] & 15) as usize) * 4;
    let total = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    // Fragmented datagrams cannot be interpreted independently as RTP packets.
    if ip[0] >> 4 != 4
        || header < 20
        || total < header + 8
        || total > ip.len()
        || ip[9] != 17
        || ip[16..20] != DESTINATION
        || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0
    {
        return;
    }
    let udp = &ip[header..total];
    let udp_length = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if u16::from_be_bytes([udp[2], udp[3]]) != DESTINATION_PORT
        || udp_length < 20
        || udp_length > udp.len()
    {
        return;
    }
    let rtp = &udp[8..udp_length];
    if rtp[0] >> 6 != 2 || rtp[1] & 127 != 33 {
        return;
    }
    let mut offset = 12 + 4 * (rtp[0] & 15) as usize;
    if offset > rtp.len() {
        return;
    }
    if rtp[0] & 0x10 != 0 {
        if offset + 4 > rtp.len() {
            return;
        }
        offset += 4 + 4 * u16::from_be_bytes([rtp[offset + 2], rtp[offset + 3]]) as usize;
    }
    let end = if rtp[0] & 0x20 != 0 {
        let padding = rtp[rtp.len() - 1] as usize;
        if padding == 0 || padding > rtp.len() {
            return;
        }
        rtp.len() - padding
    } else {
        rtp.len()
    };
    if offset > end {
        return;
    }
    for ts in rtp[offset..end].as_chunks::<188>().0 {
        if ts[0] != 0x47
            || ts[1] & 0xc0 != 0x40
            || ts[3] & 0xc0 != 0
            || u16::from_be_bytes([ts[1] & 31, ts[2]]) != VIDEO_PID
        {
            continue;
        }
        let position = match (ts[3] >> 4) & 3 {
            1 => 4,
            3 => 5 + ts[4] as usize,
            _ => continue,
        };
        if position + 14 > ts.len() {
            continue;
        }
        let pes = &ts[position..];
        if pes[..3] != [0, 0, 1]
            || !(0xe0..=0xef).contains(&pes[3])
            || pes[6] & 0xc0 != 0x80
            || pes[7] & 0x80 == 0
            || pes[8] < 5
        {
            continue;
        }
        let pts = &pes[9..14];
        if !matches!(pts[0] >> 4, 2 | 3) || pts[0] & 1 == 0 || pts[2] & 1 == 0 || pts[4] & 1 == 0 {
            continue;
        }
        emit(
            (((pts[0] >> 1) & 7) as u64) << 30
                | (pts[1] as u64) << 22
                | ((pts[2] >> 1) as u64) << 15
                | (pts[3] as u64) << 7
                | (pts[4] >> 1) as u64,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(pts: u64) -> Vec<u8> {
        let mut frame = vec![0u8; 14 + 20 + 8 + 12 + 188];
        frame[12..14].copy_from_slice(&[8, 0]);
        let ip = &mut frame[14..34];
        ip[0] = 0x45;
        ip[9] = 17;
        ip[2..4].copy_from_slice(&(228u16).to_be_bytes());
        ip[16..20].copy_from_slice(&DESTINATION);
        frame[36..38].copy_from_slice(&DESTINATION_PORT.to_be_bytes());
        frame[38..40].copy_from_slice(&(208u16).to_be_bytes());
        frame[42] = 0x80;
        frame[43] = 33;
        let ts = &mut frame[54..];
        ts.fill(0xff);
        ts[..4].copy_from_slice(&[0x47, 0x50, 0x11, 0x10]);
        ts[4..13].copy_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5]);
        ts[13..18].copy_from_slice(&[
            0x21 | (((pts >> 30) & 7) as u8) << 1,
            (pts >> 22) as u8,
            (((pts >> 15) & 127) as u8) << 1 | 1,
            (pts >> 7) as u8,
            ((pts & 127) as u8) << 1 | 1,
        ]);
        frame
    }
    fn parsed(bytes: &[u8]) -> Vec<u64> {
        let mut result = vec![];
        video_pts(bytes, |pts| result.push(pts));
        result
    }
    #[test]
    fn extracts_full_33_bit_pts_and_deduplicates() {
        for pts in [0, 1500, 90000, PTS_MASK] {
            assert_eq!(parsed(&packet(pts)), vec![pts]);
        }
        let mut stats = Statistics::default();
        stats.record(90000, 1.0);
        stats.record(90000, 1.001);
        stats.record(91500, 1.0 + 1.0 / 60.0);
        assert_eq!(stats.seen.len(), 2);
        assert!(stats.summary().unwrap().contains("timestamp-fps=60.00"));
        assert!(stats.summary().unwrap().contains("wall-fps=60.00"));
    }
    #[test]
    fn rejects_unrelated_malformed_and_truncated_packets() {
        let good = packet(12345);
        for end in 0..good.len() {
            assert!(parsed(&good[..end]).is_empty());
        }
        for (index, value) in [
            (30, 193),
            (37, 0),
            (43, 96),
            (20, 0x20),
            (56, 0x12),
            (57, 0x20),
            (67, 0x20),
            (38, 0xff),
        ] {
            let mut bad = good.clone();
            bad[index] = value;
            assert!(parsed(&bad).is_empty(), "mutation {index}");
        }
    }
    #[test]
    fn handles_rtp_extension_and_adaptation_field() {
        let mut frame = packet(90000);
        frame[42] |= 0x10;
        frame.splice(54..54, [0xab, 0xcd, 0, 1, 0, 0, 0, 0]);
        frame[16..18].copy_from_slice(&(236u16).to_be_bytes());
        frame[38..40].copy_from_slice(&(216u16).to_be_bytes());
        assert_eq!(parsed(&frame), vec![90000]);
        let mut frame = packet(45000);
        let ts = &mut frame[54..];
        ts.copy_within(4..182, 10);
        ts[3] = 0x30;
        ts[4] = 5;
        ts[5..10].fill(0);
        assert_eq!(parsed(&frame), vec![45000]);
        frame[58] = 255;
        assert!(parsed(&frame).is_empty());
    }
    #[test]
    fn handles_timestamp_wrap_without_retaining_payload() {
        let mut stats = Statistics::default();
        stats.record(PTS_MASK - 749, 0.0);
        stats.record(750, 1.0 / 60.0);
        assert!(stats.summary().unwrap().contains("timestamp-fps=60.00"));
    }
}
