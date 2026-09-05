use crate::wfd::Profile;
use anyhow::{Context, Result, bail, ensure};
use gst::prelude::*;
use gstreamer as gst;
use std::{
    net::SocketAddr,
    os::fd::RawFd,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub enum Encoder {
    Auto,
    Vaapi,
    X264,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub profile: Profile,
    pub encoder: Encoder,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub idr_request_capability: bool,
    pub pcr_lead_ms: u32,
}

pub enum Source<'a> {
    Desktop {
        fd: RawFd,
        node: u32,
        audio_sink: Option<&'a str>,
    },
    Test {
        audio: bool,
    },
}

pub struct Media {
    pipeline: gst::Pipeline,
    frames: Arc<AtomicU64>,
    started: Option<Instant>,
}

impl Media {
    pub fn new(
        settings: &Settings,
        source: Source<'_>,
        target: SocketAddr,
        rtcp: Option<SocketAddr>,
    ) -> Result<Self> {
        gst::init()?;
        let use_hevc = matches!(settings.profile, Profile::Hevc);
        let high = matches!(settings.profile, Profile::Rhp2);
        let use_va = match settings.encoder {
            Encoder::Vaapi => true,
            Encoder::X264 => false,
            Encoder::Auto => {
                gst::ElementFactory::find(if use_hevc { "vah265enc" } else { "vah264enc" })
                    .is_some()
            }
        };
        ensure!(!use_hevc || use_va, "HEVC requires the hardware VA encoder");
        // VA encoders cap GOP length at 1024; zero means automatic, not disabled.
        // Keep receiver-requested recovery enabled even with the longest GOP.
        let gop_frames = if settings.idr_request_capability {
            if use_va { 1024 } else { i32::MAX as u32 }
        } else {
            settings.fps
        };
        crate::status(&format!(
            "Encoder keyframe interval: {gop_frames} frames; receiver IDR requests={}",
            settings.idr_request_capability
        ));
        let encoder = if use_va {
            format!(
                "{} name=encoder rate-control={} bitrate=4096 cpb-size=204 b-frames=0 b-pyramid=false ref-frames=1 target-usage=7 aud=true key-int-max={} num-slices=1 qos=true {}",
                if use_hevc { "vah265enc" } else { "vah264enc" },
                if use_hevc { "vcm" } else { "cbr" },
                gop_frames,
                if use_hevc {
                    String::new()
                } else {
                    format!("cabac={} dct8x8=false", high)
                }
            )
        } else {
            format!(
                "x264enc name=encoder tune=zerolatency speed-preset=veryfast bitrate=4096 vbv-buf-capacity=50 bframes=0 ref=1 key-int-max={} cabac={} dct8x8=false",
                gop_frames, high
            )
        };
        let encoded = if use_hevc {
            "h265parse config-interval=-1 ! video/x-h265,stream-format=byte-stream,alignment=au,profile=main,level=(string)4.1"
        } else if high {
            "h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au,profile=high"
        } else {
            "h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au,profile=constrained-baseline"
        };
        let desktop_capture = matches!(&source, Source::Desktop { .. });
        let (video, audio) = match source {
            Source::Desktop { fd, node, audio_sink } => (
                format!("pipewiresrc name=capture fd={fd} path={node} keepalive-time=100 do-timestamp=true provide-clock=false"),
                audio_sink.map(|sink| format!("pulsesrc name=audio_capture device={sink}.monitor provide-clock=false do-timestamp=true buffer-time=40000 latency-time=10000"))),
            Source::Test { audio } => ("videotestsrc name=capture is-live=true pattern=ball".into(),
                audio.then(|| "audiotestsrc name=audio_capture is-live=true samplesperbuffer=1024".into())),
        };
        let audio_chain = if let Some(src) = audio {
            let aac = if gst::ElementFactory::find("fdkaacenc").is_some() {
                "fdkaacenc"
            } else {
                "avenc_aac"
            };
            format!(
                "{src} ! audioresample ! audioconvert ! audio/x-raw,rate=48000,channels=2 ! {aac} name=audio_encoder ! aacparse ! audio/mpeg,mpegversion=4,stream-format=adts ! queue max-size-time=500000000 ! mux.sink_4352"
            )
        } else {
            String::new()
        };
        let rtcp_chain = rtcp.map(|addr| format!(
            "rtp.send_rtcp_src_0 ! udpsink name=rtcp_out host={} port={} close-socket=false sync=false async=false udpsrc name=rtcp_in close-socket=false caps=application/x-rtcp ! rtp.recv_rtcp_sink_0", addr.ip(), addr.port())).unwrap_or_default();
        // Aggregate each ready mux batch below, without waiting for the next AU.
        let fitted = fit_video(settings, if use_va { "NV12" } else { "I420" });
        let pipeline_text = format!(
            "rtpbin name=rtp {video} ! {fitted} ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! {encoder} ! {encoded} ! queue max-size-time=500000000 ! mux.sink_4113 mpegtsmux name=mux alignment=0 ! queue max-size-buffers=1 ! rtpmp2tpay name=pay pt=33 mtu=1400 ssrc=1 max-ptime=1000000 perfect-rtptime=false timestamp-offset=0 seqnum-offset=0 ! rtp.send_rtp_sink_0 rtp.send_rtp_src_0 ! udpsink name=network host={} port={} bind-port=50000 sync=true async=false processing-deadline=0 {rtcp_chain} {audio_chain}",
            target.ip(),
            target.port()
        );
        let pipeline = gst::parse::launch(&pipeline_text)
            .context("Construct Rust media pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("Not a pipeline"))?;
        if use_hevc {
            install_hevc_color(&pipeline)?;
        }
        ensure!(
            (0..=125).contains(&settings.pcr_lead_ms),
            "PCR lead must be 0..125ms"
        );
        let pcr_advance_ms = 125 - settings.pcr_lead_ms;
        crate::status(&format!(
            "MPEG-TS playback lead: {}ms (PCR advanced by {pcr_advance_ms}ms)",
            settings.pcr_lead_ms
        ));
        pipeline
            .by_name("mux")
            .context("TS muxer")?
            .static_pad("src")
            .context("TS output")?
            .add_probe(gst::PadProbeType::BUFFER_LIST, move |_, info| {
                if let Some(gst::PadProbeData::BufferList(list)) = info.data.take() {
                    match ready_ts_batch(&list, pcr_advance_ms) {
                        Ok(list) => info.data = Some(gst::PadProbeData::BufferList(list)),
                        Err(error) => {
                            crate::status(&format!("TS aggregation failed: {error:#}"));
                            info.flow_res = Err(gst::FlowError::Error);
                            return gst::PadProbeReturn::Handled;
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
        if rtcp.is_some() {
            let socket = std::net::UdpSocket::bind(("0.0.0.0", crate::wfd::LOCAL_RTCP_PORT))?;
            let socket = gio::Socket::from_fd(socket.into())?;
            pipeline
                .by_name("rtcp_out")
                .context("RTCP sender")?
                .set_property("socket", &socket);
            pipeline
                .by_name("rtcp_in")
                .context("RTCP receiver")?
                .set_property("socket", &socket);
        }
        if desktop_capture {
            declare_live_capture(
                &pipeline
                    .by_name("capture")
                    .context("Desktop source")?
                    .static_pad("src")
                    .context("Desktop source pad")?,
            );
        }
        pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        if std::env::var_os("REMOTE_SCREEN_TRACE_LATENCY").is_some() {
            crate::latency::install(&pipeline)?;
        }
        let frames = Arc::new(AtomicU64::new(0));
        let count = frames.clone();
        let fit = pipeline.by_name("fit").context("Aspect-ratio scaler")?;
        let pad = pipeline
            .by_name("encoder")
            .context("Encoder missing")?
            .static_pad("src")
            .context("Encoder output pad")?;
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _| {
            let n = count.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 180 {
                for direction in ["sink", "src"] {
                    if let Some(caps) = fit.static_pad(direction).and_then(|p| p.current_caps()) {
                        crate::status(&format!("Aspect-ratio scaler {direction}: {caps}"));
                    }
                }
                crate::status(&format!(
                    "Encoded 180 frames: {}",
                    pad.current_caps()
                        .map(|c| c.to_string())
                        .unwrap_or_default()
                ));
            }
            gst::PadProbeReturn::Ok
        });
        Ok(Self {
            pipeline,
            frames,
            started: None,
        })
    }

    pub fn play(&mut self) -> Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        self.started.get_or_insert_with(Instant::now);
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Paused)?;
        Ok(())
    }

    pub fn keyframe(&self) {
        if let Some(pad) = self
            .pipeline
            .by_name("encoder")
            .and_then(|e| e.static_pad("src"))
        {
            pad.send_event(
                gstreamer_video::UpstreamForceKeyUnitEvent::builder()
                    .all_headers(true)
                    .build(),
            );
        }
    }

    pub fn poll(&self) -> Result<()> {
        let bus = self.pipeline.bus().context("Media bus missing")?;
        while let Some(message) = bus.pop() {
            match message.view() {
                gst::MessageView::Error(error) => {
                    bail!("Media error: {} ({:?})", error.error(), error.debug())
                }
                gst::MessageView::Eos(..) => bail!("Screen capture ended"),
                gst::MessageView::Latency(..) => {
                    self.pipeline.recalculate_latency()?;
                }
                _ => (),
            }
        }
        Ok(())
    }

    pub fn frame_count(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn smoke(settings: &Settings, seconds: u64, audio: bool) -> Result<()> {
        let mut settings = settings.clone();
        settings.idr_request_capability = true;
        let settings = &settings;
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0")?;
        receiver.set_read_timeout(Some(Duration::from_millis(200)))?;
        let rtcp_receiver = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let mut media = Self::new(
            settings,
            Source::Test { audio },
            receiver.local_addr()?,
            Some(rtcp_receiver.local_addr()?),
        )?;
        let encoder = media
            .pipeline
            .by_name("encoder")
            .context("Encoder missing")?;
        let interval = encoder.property::<u32>("key-int-max");
        ensure!(
            interval
                == if encoder.factory().is_some_and(|f| f.name() == "x264enc") {
                    i32::MAX as u32
                } else {
                    1024
                },
            "Receiver IDR capability did not configure the longest GOP"
        );
        let keyframes = Arc::new(AtomicU64::new(0));
        let observed_keyframes = keyframes.clone();
        encoder
            .static_pad("src")
            .context("Encoder output")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer()
                    && !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
                {
                    observed_keyframes.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
        media.play()?;
        let started = Instant::now();
        let mut requested_keyframe = false;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut packets = 0;
        let mut full_packets = 0;
        let mut video_pes = 0;
        let mut audio_pes = 0;
        let mut clock_lead_checks = 0;
        let mut video_type = None;
        let mut audio_type = None;
        let mut buf = [0u8; 1600];
        while Instant::now() < deadline {
            media.poll()?;
            if seconds >= 3 && !requested_keyframe && started.elapsed() >= Duration::from_secs(1) {
                media.keyframe();
                requested_keyframe = true;
            }
            match receiver.recv(&mut buf) {
                Ok(n) => {
                    ensure!(
                        (200..=1328).contains(&n) && buf[1] & 0x7f == 33,
                        "RTP output must group up to seven TS packets below the MTU"
                    );
                    packets += 1;
                    if n == 1328 {
                        full_packets += 1;
                    }
                    ensure!(
                        buf[0] == 0x80 && (n - 12) % 188 == 0,
                        "Unexpected RTP header or partial TS packet"
                    );
                    for ts in buf[12..n].as_chunks::<188>().0 {
                        ensure!(ts[0] == 0x47, "Invalid MPEG-TS sync byte");
                        if ts[1] & 0x40 == 0 || ts[3] & 0x10 == 0 {
                            continue;
                        }
                        let pid = ((ts[1] as u16 & 0x1f) << 8) | ts[2] as u16;
                        let offset = 4 + if ts[3] & 0x20 != 0 {
                            1 + ts[4] as usize
                        } else {
                            0
                        };
                        if offset >= 188 {
                            continue;
                        }
                        let payload = &ts[offset..];
                        if payload.starts_with(&[0, 0, 1]) {
                            if matches!(pid, 4113 | 4352)
                                && payload.len() >= 14
                                && payload[7] & 0x80 != 0
                                && ts[3] & 0x20 != 0
                                && ts[4] >= 7
                                && ts[5] & 0x10 != 0
                            {
                                let p = &payload[9..14];
                                let pts = (u64::from(p[0] & 14) << 29)
                                    | (u64::from(p[1]) << 22)
                                    | (u64::from(p[2] & 254) << 14)
                                    | (u64::from(p[3]) << 7)
                                    | u64::from(p[4] >> 1);
                                let pcr = (u64::from(ts[6]) << 25)
                                    | (u64::from(ts[7]) << 17)
                                    | (u64::from(ts[8]) << 9)
                                    | (u64::from(ts[9]) << 1)
                                    | u64::from(ts[10] >> 7);
                                let lead = pts.wrapping_sub(pcr) & ((1u64 << 33) - 1);
                                ensure!(
                                    lead == u64::from(settings.pcr_lead_ms) * 90,
                                    "Unexpected PES/PCR lead: {lead} ticks"
                                );
                                clock_lead_checks += 1;
                            }
                            if pid == 4113 {
                                video_pes += 1;
                            }
                            if pid == 4352 {
                                audio_pes += 1;
                            }
                        }
                        if pid == 32 && payload.len() > 1 {
                            let pmt = &payload[(1 + payload[0] as usize).min(payload.len())..];
                            if pmt.len() < 12 || pmt[0] != 2 {
                                continue;
                            }
                            let end = (((pmt[1] as usize & 15) << 8) | pmt[2] as usize) + 3 - 4;
                            let mut offset =
                                12 + (((pmt[10] as usize & 15) << 8) | pmt[11] as usize);
                            while offset + 5 <= end.min(pmt.len()) {
                                let pid =
                                    ((pmt[offset + 1] as u16 & 31) << 8) | pmt[offset + 2] as u16;
                                if pid == 4113 {
                                    video_type = Some(pmt[offset]);
                                }
                                if pid == 4352 {
                                    audio_type = Some(pmt[offset]);
                                }
                                offset += 5
                                    + (((pmt[offset + 3] as usize & 15) << 8)
                                        | pmt[offset + 4] as usize);
                            }
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        ensure!(
            full_packets > 0,
            "TS batches were not aggregated into full RTP packets"
        );
        if requested_keyframe {
            let keys = keyframes.load(Ordering::Relaxed);
            ensure!(keys >= 2, "Requested recovery IDR was not emitted");
            let max_keys = 2 + media.frame_count() / interval as u64;
            ensure!(keys <= max_keys, "Unexpected periodic keyframes: {keys}");
            crate::status(&format!(
                "Keyframes verified: {keys}, including requested recovery IDR"
            ));
        }
        ensure!(
            media.frame_count() > settings.fps as u64 * seconds / 2 && packets > 0,
            "Media did not produce enough live frames"
        );
        ensure!(
            video_pes > 10
                && if audio {
                    audio_pes > 10
                } else {
                    audio_pes == 0
                },
            "Unexpected video/audio PES counts"
        );
        ensure!(
            video_type
                == Some(if matches!(settings.profile, Profile::Hevc) {
                    0x24
                } else {
                    0x1b
                }),
            "Unexpected video PMT stream type: {video_type:?}"
        );
        ensure!(
            audio_type == if audio { Some(0x0f) } else { None },
            "Unexpected audio PMT stream type: {audio_type:?}"
        );
        ensure!(clock_lead_checks > 0, "No PES/PCR playback lead observed");
        crate::status(&format!(
            "Playback lead verified: {}ms in {clock_lead_checks} PES/PCR pairs",
            settings.pcr_lead_ms
        ));
        crate::status(&format!(
            "TS verified: {video_pes} video PES, {audio_pes} AAC PES; video type={video_type:?}, audio type={audio_type:?}"
        ));
        crate::status(&format!(
            "PASS {:?}: {} frames, {packets} RTP packets in {seconds}s, AAC {}",
            settings.profile,
            media.frame_count(),
            if audio { "enabled" } else { "disabled" }
        ));
        Ok(())
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

// PipeWire 1.6.8 publishes caps before initializing its private is_live flag.
// VA encoders query latency during caps setup and otherwise choose non-live
// buffering. Correct only the live bit after the source answers the query;
// retain its actual latency bounds, including later runtime updates.
fn declare_live_capture(pad: &gst::Pad) {
    let logged = std::sync::atomic::AtomicBool::new(false);
    pad.add_probe(gst::PadProbeType::QUERY_UPSTREAM | gst::PadProbeType::PULL, move |_, info| {
        if let Some(query) = info.query_mut()
            && let gst::QueryViewMut::Latency(latency) = query.view_mut()
        {
            let (live, min, max) = latency.result();
            if !live {
                latency.set(true, min, max);
                if !logged.swap(true, Ordering::Relaxed) {
                    crate::status("Corrected PipeWire startup latency query to live capture; latency bounds preserved");
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
}

// Patch before h265parse caches parameter sets for later recovery keyframes.
fn install_hevc_color(pipeline: &gst::Pipeline) -> Result<()> {
    let pad = pipeline
        .by_name("encoder")
        .context("HEVC encoder")?
        .static_pad("src")
        .context("HEVC output pad")?;
    pad.add_probe(gst::PadProbeType::BUFFER, |_, info| {
        let Some(buffer) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };
        let result = (|| -> Result<Option<gst::Buffer>> {
            let map = buffer.map_readable()?;
            let Some(bytes) = crate::hevc_color::patch_access_unit(map.as_slice())? else {
                return Ok(None);
            };
            let mut output = gst::Buffer::from_mut_slice(bytes);
            buffer.copy_into(
                output.get_mut().context("Writable HEVC metadata")?,
                gst::BUFFER_COPY_METADATA,
                ..,
            )?;
            Ok(Some(output))
        })();
        match result {
            Ok(Some(buffer)) => info.data = Some(gst::PadProbeData::Buffer(buffer)),
            Ok(None) => {}
            Err(error) => {
                crate::status(&format!("HEVC color metadata failed: {error:#}"));
                info.flow_res = Err(gst::FlowError::Error);
                return gst::PadProbeReturn::Handled;
            }
        }
        gst::PadProbeReturn::Ok
    });
    crate::status("HEVC color metadata: BT.709, limited range; SPS correction enabled");
    Ok(())
}

// PipeWire exposes a PAR range that can otherwise fixate to 1/i32::MAX.
// Desktop pixels are square: constrain the source before scaling or negotiating.
fn fit_video(settings: &Settings, format: &str) -> String {
    let color = if matches!(settings.profile, Profile::Hevc) && format == "NV12" {
        ",colorimetry=bt709"
    } else {
        ""
    };
    format!(
        "video/x-raw,pixel-aspect-ratio=1/1 ! videorate drop-only=true ! videoscale name=fit add-borders=true n-threads=2 ! video/x-raw,width={},height={},framerate={}/1,pixel-aspect-ratio=1/1 ! videoconvert n-threads=2 ! video/x-raw,format={format}{color}",
        settings.width, settings.height, settings.fps
    )
}

// mpegtsmux alignment=0 finishes each ready AU as a list of TS buffers. Merge
// only that list: rtpmp2tpay splits it into MTU-sized packets and flushes its tail.
// A single TS needs an empty input to flush immediately in GStreamer 1.28.
fn ready_ts_batch(list: &gst::BufferListRef, pcr_advance_ms: u32) -> Result<gst::BufferList> {
    let Some(first) = list.get(0) else {
        return Ok(gst::BufferList::new());
    };
    let size: usize = list.iter().map(|b| b.size()).sum();
    let mut buffer = gst::Buffer::with_size(size)?;
    let output = buffer.get_mut().context("Writable TS batch")?;
    output.set_pts(first.pts());
    output.set_dts(first.dts());
    output.set_duration(first.duration());
    output.set_flags(first.flags());
    {
        let mut bytes = output.map_writable()?;
        let mut offset = 0;
        for part in list.iter() {
            let data = part.map_readable()?;
            bytes[offset..offset + data.len()].copy_from_slice(&data);
            offset += data.len();
        }
        if pcr_advance_ms != 0 {
            crate::transport_clock::advance_pcr(bytes.as_mut_slice(), pcr_advance_ms)?;
        }
    }
    let mut result: gst::BufferList = [buffer].into_iter().collect();
    if size == 188 {
        let mut flush = gst::Buffer::new();
        flush
            .get_mut()
            .context("Flush buffer")?
            .set_pts(first.pts());
        result.get_mut().context("Writable TS list")?.add(flush);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hevc_color_probe_preserves_timing_and_forwards_without_another_frame() {
        gst::init().unwrap();
        let pipeline = gst::parse::launch("appsrc name=input is-live=true format=time ! identity name=encoder ! appsink name=output sync=false").unwrap().downcast::<gst::Pipeline>().unwrap();
        install_hevc_color(&pipeline).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let hex = "0000000142010101600000030090000003000003007ba003c0801107cbb2e491b6affc0004000404000003000400000300f020";
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let expected = crate::hevc_color::patch_access_unit(&bytes)
            .unwrap()
            .unwrap();
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let b = buffer.get_mut().unwrap();
        b.set_pts(gst::ClockTime::from_mseconds(123));
        b.set_dts(gst::ClockTime::from_mseconds(120));
        b.set_duration(gst::ClockTime::from_mseconds(16));
        b.set_offset(7);
        b.set_offset_end(8);
        b.set_flags(gst::BufferFlags::HEADER | gst::BufferFlags::MARKER);
        let input = pipeline.by_name("input").unwrap();
        assert_eq!(
            input.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]),
            gst::FlowReturn::Ok
        );
        let sample = pipeline
            .by_name("output")
            .unwrap()
            .emit_by_name::<Option<gst::Sample>>(
                "try-pull-sample",
                &[&gst::ClockTime::from_seconds(1)],
            )
            .expect("SPS correction waited for another frame");
        let out = sample.buffer().unwrap();
        assert_eq!(out.map_readable().unwrap().as_slice(), expected);
        assert_eq!(out.pts(), buffer.pts());
        assert_eq!(out.dts(), buffer.dts());
        assert_eq!(out.duration(), buffer.duration());
        assert_eq!(out.offset(), 7);
        assert_eq!(out.offset_end(), 8);
        assert!(
            out.flags()
                .contains(gst::BufferFlags::HEADER | gst::BufferFlags::MARKER)
        );
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn variable_rate_desktop_frame_is_forwarded_without_waiting_for_another_frame() {
        gst::init().unwrap();
        let settings = Settings {
            profile: Profile::Hevc,
            encoder: Encoder::Vaapi,
            width: 1280,
            height: 720,
            fps: 60,
            idr_request_capability: true,
            pcr_lead_ms: 125,
        };
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=input is-live=true format=time caps=video/x-raw,format=RGB,width=160,height=100,framerate=0/1,max-framerate=120/1 ! {} ! appsink name=output sync=false",
            fit_video(&settings, "RGB")
        )).unwrap().downcast::<gst::Pipeline>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let input = pipeline.by_name("input").unwrap();
        let output = pipeline.by_name("output").unwrap();
        for timestamp in [0, 100] {
            let mut buffer = gst::Buffer::from_mut_slice(vec![255; 160 * 100 * 3]);
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(timestamp));
            assert_eq!(
                input.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]),
                gst::FlowReturn::Ok
            );
            // No subsequent input or EOS is supplied while waiting for this frame.
            let sample = output
                .emit_by_name::<Option<gst::Sample>>(
                    "try-pull-sample",
                    &[&gst::ClockTime::from_seconds(1)],
                )
                .expect("Desktop update waited for a later frame");
            assert_eq!(
                sample.buffer().unwrap().pts(),
                Some(gst::ClockTime::from_mseconds(timestamp))
            );
        }
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn live_capture_correction_preserves_source_latency_bounds() {
        gst::init().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src)
            .query_function(|_, _, query| {
                if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                    latency.set(
                        false,
                        gst::ClockTime::from_mseconds(7),
                        gst::ClockTime::from_mseconds(40),
                    );
                    true
                } else {
                    false
                }
            })
            .build();
        declare_live_capture(&pad);
        let mut query = gst::query::Latency::new();
        assert!(pad.query(&mut query));
        assert_eq!(
            query.result(),
            (
                true,
                gst::ClockTime::from_mseconds(7),
                Some(gst::ClockTime::from_mseconds(40))
            )
        );
    }

    #[test]
    fn desktop_aspect_ratio_has_black_borders_and_preserves_all_four_edges() {
        gst::init().unwrap();
        let settings = Settings {
            profile: Profile::Hevc,
            encoder: Encoder::Vaapi,
            width: 1920,
            height: 1080,
            fps: 60,
            idr_request_capability: true,
            pcr_lead_ms: 125,
        };
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=input format=time caps=video/x-raw,format=RGB,width=2880,height=1800,framerate=60/1 ! {} ! appsink name=output sync=false",
            fit_video(&settings, "RGB")
        )).unwrap().downcast::<gst::Pipeline>().unwrap();
        // Reproduce PipeWire's wide PAR offer: the upstream constraint must
        // collapse it to square pixels before the source fixates its caps.
        let offered = "video/x-raw,format=RGB,width=2880,height=1800,framerate=60/1,pixel-aspect-ratio=(fraction)[1/2147483647,2147483647/1]".parse::<gst::Caps>().unwrap();
        let mut accepted = pipeline
            .by_name("input")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .peer_query_caps(Some(&offered));
        accepted.fixate();
        assert_eq!(
            accepted
                .structure(0)
                .unwrap()
                .get::<gst::Fraction>("pixel-aspect-ratio")
                .unwrap(),
            gst::Fraction::new(1, 1)
        );
        // White desktop with a different marker on every edge detects cropping.
        let mut pixels = vec![255; 2880 * 1800 * 3];
        for y in 0..1800 {
            for x in 0..2880 {
                let color = if y < 16 {
                    [0, 0, 255]
                } else if y >= 1784 {
                    [255, 255, 0]
                } else if x < 16 {
                    [255, 0, 0]
                } else if x >= 2864 {
                    [0, 255, 0]
                } else {
                    [255, 255, 255]
                };
                pixels[(y * 2880 + x) * 3..(y * 2880 + x + 1) * 3].copy_from_slice(&color);
            }
        }
        let mut buffer = gst::Buffer::from_mut_slice(pixels);
        buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        buffer
            .get_mut()
            .unwrap()
            .set_duration(gst::ClockTime::SECOND / 60);
        pipeline.set_state(gst::State::Playing).unwrap();
        let input = pipeline.by_name("input").unwrap();
        assert_eq!(
            input.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]),
            gst::FlowReturn::Ok
        );
        assert_eq!(
            input.emit_by_name::<gst::FlowReturn>("end-of-stream", &[]),
            gst::FlowReturn::Ok
        );
        let sample = pipeline
            .by_name("output")
            .unwrap()
            .emit_by_name::<Option<gst::Sample>>(
                "try-pull-sample",
                &[&gst::ClockTime::from_seconds(5)],
            )
            .expect("Scaled frame missing");
        let caps = sample.caps().unwrap();
        let info = gstreamer_video::VideoInfo::from_caps(caps).unwrap();
        assert_eq!(
            (info.width(), info.height(), info.par()),
            (1920, 1080, gst::Fraction::new(1, 1))
        );
        let data = sample.buffer().unwrap().map_readable().unwrap();
        let stride = info.stride()[0] as usize;
        let pixel = |x: usize, y: usize| &data[y * stride + x * 3..y * stride + x * 3 + 3];
        for y in 0..1080 {
            for x in (0..96).chain(1824..1920) {
                assert_eq!(pixel(x, y), [0, 0, 0]);
            }
        }
        assert_eq!(pixel(99, 540), [255, 0, 0]);
        assert_eq!(pixel(1820, 540), [0, 255, 0]);
        assert_eq!(pixel(960, 3), [0, 0, 255]);
        assert_eq!(pixel(960, 1076), [255, 255, 0]);
        assert_eq!(pixel(960, 540), [255, 255, 255]);
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn rtp_flushes_small_updates_and_large_frame_tails_without_another_frame() {
        gst::init().unwrap();
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let pipeline = gst::parse::launch(&format!(
            "appsrc name=input is-live=true format=time caps=video/mpegts,packetsize=188,systemstream=true ! rtpmp2tpay pt=33 mtu=1400 max-ptime=1000000 timestamp-offset=0 seqnum-offset=0 ! udpsink host=127.0.0.1 port={} sync=false async=false",
            receiver.local_addr().unwrap().port()
        )).unwrap().downcast::<gst::Pipeline>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let input = pipeline.by_name("input").unwrap();
        for count in [1usize, 3, 15] {
            let list: gst::BufferList = (0..count)
                .map(|i| {
                    let mut buffer = gst::Buffer::from_mut_slice(vec![i as u8; 188]);
                    buffer
                        .get_mut()
                        .unwrap()
                        .set_pts(gst::ClockTime::from_mseconds(42));
                    buffer
                })
                .collect();
            let batch = ready_ts_batch(&list, 0).unwrap();
            for buffer in batch.iter() {
                assert_eq!(
                    input.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer.to_owned()]),
                    gst::FlowReturn::Ok
                );
            }
            let mut received = Vec::new();
            let mut packets = 0;
            while received.len() < count * 188 {
                let mut packet = [0u8; 1600];
                let size = receiver
                    .recv(&mut packet)
                    .expect("Tail waited for a later frame");
                assert!((200..=1328).contains(&size));
                assert_eq!((size - 12) % 188, 0);
                received.extend_from_slice(&packet[12..size]);
                packets += 1;
            }
            assert_eq!(packets, count.div_ceil(7));
            for i in 0..count {
                assert_eq!(&received[i * 188..(i + 1) * 188], &[i as u8; 188]);
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn ts_batches_preserve_bytes_and_timestamp_without_waiting_for_more() {
        gst::init().unwrap();
        for count in [1, 3, 7, 15] {
            let list: gst::BufferList = (0..count)
                .map(|i| {
                    let mut buffer = gst::Buffer::from_mut_slice(vec![i as u8; 188]);
                    buffer
                        .get_mut()
                        .unwrap()
                        .set_pts(gst::ClockTime::from_mseconds(42));
                    buffer
                })
                .collect();
            let out = ready_ts_batch(&list, 0).unwrap();
            assert_eq!(out.len(), if count == 1 { 2 } else { 1 });
            let batch = out.get(0).unwrap();
            assert_eq!(batch.pts(), Some(gst::ClockTime::from_mseconds(42)));
            let bytes = batch.map_readable().unwrap();
            assert_eq!(bytes.len(), count * 188);
            for i in 0..count {
                assert_eq!(&bytes[i * 188..(i + 1) * 188], &[i as u8; 188]);
            }
            if count == 1 {
                assert_eq!(out.get(1).unwrap().size(), 0);
            }
        }
    }
}
