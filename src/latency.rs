//! Optional sender diagnostics: stage age relative to running PTS, not display latency.
use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex, mpsc},
};

const WINDOW: usize = 300;

#[derive(Default)]
struct Samples {
    seen: HashSet<u64>,
    ages: Vec<i64>,
    last_pts: Option<u64>,
}

impl Samples {
    fn record(&mut self, pts: u64, age: i64) -> Option<[i64; 3]> {
        if self.last_pts == Some(pts) {
            return None;
        }
        self.last_pts = Some(pts);
        if !self.seen.insert(pts) {
            return None;
        }
        self.ages.push(age);
        if self.ages.len() < WINDOW {
            return None;
        }
        self.ages.sort_unstable();
        let result = [
            self.ages[WINDOW.div_ceil(2) - 1],
            self.ages[(WINDOW * 95).div_ceil(100) - 1],
            self.ages[WINDOW - 1],
        ];
        self.seen.clear();
        self.ages.clear();
        Some(result)
    }
}

// GStreamer 1.28.6 tsmux.c adds CLOCK_BASE (90 kHz * 3600 s) to PES PTS.
const PTS_MASK: u64 = (1 << 33) - 1;
const PES_OFFSET: u64 = 90_000 * 3600;
const MAX_FRAMES: usize = 1024;
const PAIRS: [(&str, usize, usize); 7] = [
    ("capture->encoder-input", 0, 1),
    ("encoder-input->output", 1, 2),
    ("encoder-output->mux-input", 2, 3),
    ("mux-input->video-PES", 3, 4),
    ("encoder-output->video-PES", 2, 4),
    ("video-PES->network-sink", 4, 5),
    ("capture->network-sink", 0, 5),
];

#[derive(Default)]
struct Paired {
    frames: BTreeMap<u64, [Option<u64>; 6]>,
    samples: [Samples; 7],
    pes_age: Samples,
    mux_offset: Samples,
}

impl Paired {
    fn record(&mut self, pts: u64, stage: usize, now: u64) -> Vec<(&'static str, [i64; 3])> {
        let times = self.frames.entry(pts).or_insert([None; 6]);
        if times[stage].is_some() {
            return Vec::new();
        }
        times[stage] = Some(now);
        let mut reports = Vec::new();
        for (index, &(label, start, end)) in PAIRS.iter().enumerate() {
            if end == stage
                && let (Some(start), Some(end)) = (times[start], times[end])
                && let Some(elapsed) = end.checked_sub(start)
                && let Ok(elapsed) = i64::try_from(elapsed)
                && let Some(summary) = self.samples[index].record(pts, elapsed)
            {
                reports.push((label, summary));
            }
        }
        if self.frames.len() > MAX_FRAMES {
            self.frames.pop_first();
        }
        reports
    }
}

fn pts_90k(ns: u64) -> u64 {
    ((u128::from(ns) * 90_000 / 1_000_000_000) as u64) & PTS_MASK
}

// Only inspect the PES header of video PID 4113. No payload is retained.
fn video_pts(packet: &[u8]) -> Option<u64> {
    if packet.len() != 188
        || packet[0] != 0x47
        || packet[1] & 0xc0 != 0x40
        || ((u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2])) != 4113
        || packet[3] & 0x10 == 0
    {
        return None;
    }
    let offset = if packet[3] & 0x20 != 0 {
        5 + usize::from(packet[4])
    } else {
        4
    };
    let pes = packet.get(offset..)?;
    if pes.len() < 14
        || pes[..3] != [0, 0, 1]
        || !(0xe0..=0xef).contains(&pes[3])
        || pes[7] & 0x80 == 0
        || pes[8] < 5
    {
        return None;
    }
    let p = &pes[9..14];
    if p[0] & 1 == 0 || p[2] & 1 == 0 || p[4] & 1 == 0 {
        return None;
    }
    let pts = (u64::from(p[0] & 0x0e) << 29)
        | (u64::from(p[1]) << 22)
        | (u64::from(p[2] & 0xfe) << 14)
        | (u64::from(p[3]) << 7)
        | u64::from(p[4] >> 1);
    Some(pts.wrapping_sub(PES_OFFSET) & PTS_MASK)
}

fn rtp_payload(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 12 || data[0] >> 6 != 2 || data[1] & 0x7f != 33 {
        return None;
    }
    let mut start = 12 + usize::from(data[0] & 0xf) * 4;
    if data[0] & 0x10 != 0 {
        let header = data.get(start..start + 4)?;
        start += 4 + usize::from(u16::from_be_bytes([header[2], header[3]])) * 4;
    }
    let end = if data[0] & 0x20 != 0 {
        data.len().checked_sub(usize::from(*data.last()?))?
    } else {
        data.len()
    };
    data.get(start..end)
}

struct Report {
    stage: &'static str,
    paired: bool,
    ages: [i64; 3],
    pts: u64,
    running_pts: u64,
    clock: u64,
    base_time: u64,
}

/// Install only when explicitly requested by the caller. Probes retain no frame data.
pub fn install(pipeline: &gst::Pipeline) -> Result<()> {
    let stages = [
        ("capture", "src", "capture.src"),
        ("encoder", "sink", "encoder.sink"),
        ("encoder", "src", "encoder.src"),
        ("mux", "src", "mux.src"),
        ("pay", "src", "pay.src"),
        ("network", "sink", "network.sink"),
    ];
    // Resolve every pad before changing the pipeline.
    let mut pads = stages
        .into_iter()
        .map(|(element, pad, label)| {
            Ok((
                pipeline
                    .by_name(element)
                    .with_context(|| format!("Latency diagnostic element {element}"))?
                    .static_pad(pad)
                    .with_context(|| format!("Latency diagnostic pad {label}"))?,
                label,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    for (element, pad_name, label) in [
        ("mux", "sink_4113", "mux.video-input"),
        ("audio_encoder", "src", "audio_encoder.src"),
    ] {
        if let Some(pad) = pipeline
            .by_name(element)
            .and_then(|e| e.static_pad(pad_name))
        {
            pads.push((pad, label));
        }
    }
    let paired = Arc::new(Mutex::new(Paired::default()));
    let (sender, receiver) = mpsc::sync_channel::<Report>(12);
    let weak = pipeline.downgrade();
    // Querying latency from a streaming probe could contend with streaming locks.
    // This thread exits when the probes (and their senders) are dropped.
    std::thread::Builder::new()
        .name("latency-reports".into())
        .spawn(move || {
            while let Ok(report) = receiver.recv() {
                let Some(pipeline) = weak.upgrade() else {
                    break;
                };
                if report.paired {
                    crate::status(&format!(
                        "Latency paired {} n={WINDOW}: ms p50={:.3} p95={:.3} max={:.3}",
                        report.stage,
                        report.ages[0] as f64 / 1_000_000.0,
                        report.ages[1] as f64 / 1_000_000.0,
                        report.ages[2] as f64 / 1_000_000.0,
                    ));
                    continue;
                }
                let mut query = gst::query::Latency::new();
                let queried = if pipeline.query(&mut query) {
                    let (live, min, max) = query.result();
                    format!("live={live} min={min} max={max:?}")
                } else {
                    "unavailable".into()
                };
                crate::status(&format!(
                    "Latency trace {} n={WINDOW}: PTS-age ms p50={:.3} p95={:.3} max={:.3}; raw_pts_ns={} running_pts_ns={} clock_ns={} base_time_ns={}; configured_latency={:?}; query {queried}",
                    report.stage,
                    report.ages[0] as f64 / 1_000_000.0,
                    report.ages[1] as f64 / 1_000_000.0,
                    report.ages[2] as f64 / 1_000_000.0,
                    report.pts,
                    report.running_pts,
                    report.clock,
                    report.base_time,
                    pipeline.latency(),
                ));
                if report.stage == "encoder.src" {
                    for (element, pad_name, peer, label) in [
                        ("capture", "src", false, "capture"),
                        ("audio_capture", "src", false, "audio-capture"),
                        ("audio_encoder", "src", false, "audio-encoder"),
                        ("encoder", "sink", true, "encoder-upstream"),
                        ("encoder", "src", false, "encoder-output"),
                    ] {
                        let Some(pad) = pipeline
                            .by_name(element)
                            .and_then(|e| e.static_pad(pad_name))
                        else {
                            continue;
                        };
                        let mut query = gst::query::Latency::new();
                        let success = if peer {
                            pad.peer_query(&mut query)
                        } else {
                            pad.query(&mut query)
                        };
                        if success {
                            let (live, min, max) = query.result();
                            crate::status(&format!(
                                "Latency trace {label} query: live={live} min={min} max={max:?}"
                            ));
                        }
                    }
                }
            }
        })
        .context("Start latency diagnostic reporting")?;

    for (pad, stage) in pads {
        let weak = pipeline.downgrade();
        let sender = sender.clone();
        let samples = Mutex::new(Samples::default());
        let paired = paired.clone();
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |pad, info| {
                let Some(pipeline) = weak.upgrade() else {
                    return gst::PadProbeReturn::Remove;
                };
                let Some(clock) = pipeline.clock().map(|c| c.time()) else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(base_time) = pipeline.base_time() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(now) = clock.checked_sub(base_time) else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(event) = pad.sticky_event::<gst::event::Segment>(0) else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(segment) = event.segment().downcast_ref::<gst::ClockTime>() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Ok(mut samples) = samples.lock() else {
                    return gst::PadProbeReturn::Ok;
                };
                let mut record = |buffer: &gst::BufferRef| {
                    let pair_stage = match stage {
                        "capture.src" => Some(0),
                        "encoder.sink" => Some(1),
                        "encoder.src" => Some(2),
                        "mux.video-input" => Some(3),
                        "mux.src" => Some(4),
                        "network.sink" => Some(5),
                        _ => None,
                    };
                    if let Some(pair_stage) = pair_stage {
                        let record_pair = |key| {
                            if let Ok(mut paired) = paired.lock() {
                                let mut reports = paired.record(key, pair_stage, now.nseconds());
                                if pair_stage == 4 {
                                    // Unwrap the 33-bit PES clock relative to current running time.
                                    let delta_ticks = (pts_90k(now.nseconds())
                                        .wrapping_sub(key)
                                        .wrapping_add(1 << 32)
                                        & PTS_MASK)
                                        as i64
                                        - (1 << 32);
                                    let age = delta_ticks * 1_000_000_000 / 90_000;
                                    if let Some(ages) = paired.pes_age.record(key, age) {
                                        reports.push(("video-PES.source-PTS-age", ages));
                                    }
                                    if let Some(outer) =
                                        buffer.pts().and_then(|p| segment.to_running_time(p))
                                    {
                                        let offset = (pts_90k(outer.nseconds())
                                            .wrapping_sub(key)
                                            .wrapping_add(1 << 32)
                                            & PTS_MASK)
                                            as i64
                                            - (1 << 32);
                                        if let Some(ages) = paired
                                            .mux_offset
                                            .record(key, offset * 1_000_000_000 / 90_000)
                                        {
                                            reports.push(("mux-outer-minus-video-PTS", ages));
                                        }
                                    }
                                }
                                for (label, ages) in reports {
                                    let _ = sender.try_send(Report {
                                        stage: label,
                                        paired: true,
                                        ages,
                                        pts: key,
                                        running_pts: 0,
                                        clock: clock.nseconds(),
                                        base_time: base_time.nseconds(),
                                    });
                                }
                            }
                        };
                        if pair_stage >= 4 {
                            if let Ok(map) = buffer.map_readable() {
                                let data = if pair_stage == 5 {
                                    rtp_payload(map.as_slice())
                                } else {
                                    Some(map.as_slice())
                                };
                                if let Some(data) = data {
                                    for packet in data.as_chunks::<188>().0 {
                                        if let Some(key) = video_pts(packet) {
                                            record_pair(key);
                                        }
                                    }
                                }
                            }
                        } else if let Some(pts) =
                            buffer.pts().and_then(|p| segment.to_running_time(p))
                        {
                            record_pair(pts_90k(pts.nseconds()));
                        }
                    }
                    let Some(pts) = buffer.pts() else { return };
                    // Encoders may offset timestamps by hours; their output Segment
                    // maps those timestamps back to the pipeline running time.
                    let Some(running_pts) = segment.to_running_time(pts) else {
                        return;
                    };
                    let delta = i128::from(now.nseconds()) - i128::from(running_pts.nseconds());
                    let Ok(age) = i64::try_from(delta) else {
                        return;
                    };
                    if let Some(ages) = samples.record(running_pts.nseconds(), age) {
                        // A slow logger must never stall capture or packet delivery.
                        let _ = sender.try_send(Report {
                            stage,
                            paired: false,
                            ages,
                            pts: pts.nseconds(),
                            running_pts: running_pts.nseconds(),
                            clock: clock.nseconds(),
                            base_time: base_time.nseconds(),
                        });
                    }
                };
                if let Some(buffer) = info.buffer() {
                    record(buffer);
                }
                if let Some(list) = info.buffer_list() {
                    for buffer in list.iter() {
                        record(buffer);
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
    }
    crate::status(
        "Latency trace enabled: stage PTS age only, not end-to-end display latency; network.sink is before UDP sink clock waiting; paired elapsed times use identical normalized video PES PTS; at most 1024 numeric frame records, 300 samples per metric and 12 queued summaries",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pes_pts_matches_source_running_time_and_rejects_other_packets() {
        let ns = 1_234_567_890;
        let wire_pts = pts_90k(ns) + PES_OFFSET;
        let mut packet = [0xff; 188];
        packet[..4].copy_from_slice(&[0x47, 0x50, 0x11, 0x30]);
        packet[4] = 7; // adaptation field
        let pes = &mut packet[12..];
        pes[..9].copy_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5]);
        pes[9..14].copy_from_slice(&[
            0x21 | (((wire_pts >> 30) as u8 & 7) << 1),
            (wire_pts >> 22) as u8,
            ((wire_pts >> 14) as u8 & 0xfe) | 1,
            (wire_pts >> 7) as u8,
            ((wire_pts as u8) << 1) | 1,
        ]);
        assert_eq!(video_pts(&packet), Some(pts_90k(ns)));
        let mut rtp = vec![0x80, 33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        rtp.extend_from_slice(&packet);
        assert_eq!(video_pts(rtp_payload(&rtp).unwrap()), Some(pts_90k(ns)));
        packet[1] &= !0x40;
        assert_eq!(video_pts(&packet), None);
        packet[1] |= 0x40;
        packet[2] = 0;
        assert_eq!(video_pts(&packet), None);
        packet[2] = 0x11;
        packet[4] = 183;
        assert_eq!(video_pts(&packet), None);
    }

    #[test]
    fn paired_times_use_same_frame_and_bound_unmatched_frames() {
        let mut paired = Paired::default();
        for pts in 0..WINDOW as u64 {
            for stage in 0..6 {
                let reports = paired.record(pts, stage, pts * 1000 + stage as u64 * 10);
                if pts == WINDOW as u64 - 1 && stage == 5 {
                    assert!(reports.contains(&("capture->network-sink", [50; 3])));
                    assert!(reports.contains(&("video-PES->network-sink", [10; 3])));
                }
                assert!(paired.record(pts, stage, 999999).is_empty());
            }
        }
        for pts in WINDOW as u64..5000 {
            paired.record(pts, 0, pts);
            assert!(paired.frames.len() <= MAX_FRAMES);
        }
    }

    #[test]
    fn summaries_deduplicate_pts_and_bound_storage() {
        let mut samples = Samples::default();
        for cycle in 0..10 {
            for n in 0..WINDOW {
                let pts = (cycle * WINDOW + n) as u64;
                let result = samples.record(pts, n as i64 - 150);
                assert!(samples.record(pts, 100_000).is_none());
                if n == WINDOW - 1 {
                    assert_eq!(result, Some([-1, 134, 149]));
                    assert!(samples.ages.is_empty());
                    assert!(samples.seen.is_empty());
                } else {
                    assert!(result.is_none());
                    assert_eq!(samples.ages.len(), n + 1);
                    assert_eq!(samples.seen.len(), n + 1);
                }
            }
        }
    }
}
