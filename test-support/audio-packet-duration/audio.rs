//! On-demand VM sound, independent of the video ACK window.
//! Both raw and encoded queues discard stale sound instead of delaying the desktop.
//!
//! Candidate replacement for `crates/gliss-server/src/audio.rs` (research
//! preparation, not promoted): adds a typed, validated 20/40 ms packet-duration
//! policy, per-encoder frozen frame/PCM sizing, an explicit opt-in encode
//! configuration and cumulative native counters. Every pre-existing public entry
//! point keeps its signature and its 20 ms default behavior.
use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use gliss_rfb::audio::{CHANNELS, Event, SAMPLE_RATE};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

/// Frames per packet under the default (20 ms) policy; retained for callers.
pub const FRAMES: u32 = PacketDuration::DEFAULT.frames();
/// Fixed freshness budget for raw fragments, partial packets and the writer.
/// The packet-duration policy never changes it.
pub const MAX_AGE: Duration = Duration::from_millis(100);
/// Default-policy PCM length; the pipeline sizes per encoder, so only tests use it.
#[cfg(test)]
const PCM_BYTES: usize = PacketDuration::DEFAULT.pcm_bytes();

/// Packet duration policy. Only the two canonical values exist; the default is
/// the historical 20 ms. The GLA1 header carries the frame count, so the wire
/// format is unchanged for either value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PacketDuration {
    #[default]
    Ms20,
    Ms40,
}

impl PacketDuration {
    pub const DEFAULT: Self = Self::Ms20;

    pub const fn millis(self) -> u32 {
        match self {
            Self::Ms20 => 20,
            Self::Ms40 => 40,
        }
    }

    /// Frames (stereo sample pairs) per packet at `SAMPLE_RATE`.
    pub const fn frames(self) -> u32 {
        match self {
            Self::Ms20 => 960,
            Self::Ms40 => 1920,
        }
    }

    /// Exact stereo s16le byte count of one packet's PCM input.
    pub const fn pcm_bytes(self) -> usize {
        self.frames() as usize * CHANNELS as usize * 2
    }

    /// Accepts exactly 20 or 40; every other value is an error.
    pub fn from_millis(millis: u32) -> Result<Self> {
        match millis {
            20 => Ok(Self::Ms20),
            40 => Ok(Self::Ms40),
            other => bail!("audio packet duration must be 20 or 40 ms, not {other}"),
        }
    }
}

impl FromStr for PacketDuration {
    type Err = anyhow::Error;

    /// Accepts only the exact decimal strings `20` and `40`: no whitespace,
    /// sign, leading zero or unit suffix.
    fn from_str(text: &str) -> Result<Self> {
        match text {
            "20" => Ok(Self::Ms20),
            "40" => Ok(Self::Ms40),
            other => bail!("audio packet duration must be exactly \"20\" or \"40\", not {other:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Pcm,
    Opus,
    #[serde(rename = "ima-adpcm")]
    ImaAdpcm,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::Pcm => "pcm",
            Self::Opus => "opus",
            Self::ImaAdpcm => "ima-adpcm",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub codec: Option<Codec>,
    pub generation: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub available: bool,
    pub enabled: bool,
    pub codec: Option<Codec>,
    pub message: Option<String>,
}

impl Status {
    pub fn message(&self) -> Value {
        let mut value = json!({"type":"audio_status", "available":self.available, "enabled":self.enabled,
                "codecs":["opus", "ima-adpcm", "pcm"]});
        if let Some(codec) = self.codec {
            value["codec"] = codec.name().into();
        }
        if let Some(message) = &self.message {
            value["message"] = message.clone().into();
        }
        value
    }
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub generation: u64,
    pub received: Instant,
    pub bytes: Arc<[u8]>,
}

pub struct Session {
    pub request: watch::Sender<Request>,
    pub status: watch::Receiver<Status>,
    pub packets: broadcast::Sender<Packet>,
}

pub fn channel() -> (Session, watch::Receiver<Request>, watch::Sender<Status>) {
    let (request, requests) = watch::channel(Request::default());
    let (status_tx, status) = watch::channel(Status::default());
    let (packets, _) = broadcast::channel(4);
    (
        Session {
            request,
            status,
            packets,
        },
        requests,
        status_tx,
    )
}

pub fn request(sender: &watch::Sender<Request>, value: Value) -> Result<()> {
    #[derive(Deserialize)]
    struct Control {
        enabled: bool,
        codec: Option<Codec>,
    }
    let control: Control = serde_json::from_value(value).context("invalid audio control")?;
    let codec = if control.enabled {
        Some(control.codec.context("audio codec is required")?)
    } else {
        None
    };
    sender.send_if_modified(|current| {
        if current.codec == codec {
            return false;
        }
        current.codec = codec;
        current.generation = current.generation.wrapping_add(1);
        true
    });
    Ok(())
}

pub async fn update_capture(
    writer: &mut gliss_rfb::RfbWriter,
    request: Request,
    enabled: &mut bool,
    status: &watch::Sender<Status>,
    allowed: bool,
) {
    let available = allowed && *writer.audio_available().borrow();
    let desired = available && request.codec.is_some();
    let mut message = None;
    if desired != *enabled {
        match writer.set_audio(desired).await {
            Ok(()) => *enabled = desired,
            Err(error) => {
                message = Some(error.to_string());
                *enabled = false;
            }
        }
    }
    status.send_replace(Status {
        available,
        enabled: *enabled,
        codec: request.codec,
        message,
    });
}

/// Cumulative native counters owned by one packetizer task plus a separate,
/// writer-owned stale and lag group. Each field is an independent relaxed atomic:
/// a [`Counters::snapshot`] reads the fields one at a time and offers no
/// cross-field coherence. Values measure this process's own discards and
/// emissions; they are neither guest-side age nor proof of delivery.
#[derive(Debug, Default)]
pub struct Counters {
    // Packetizer-owned.
    stale_raw_events: AtomicU64,
    stale_raw_bytes: AtomicU64,
    stale_pending_discards: AtomicU64,
    stale_pending_bytes: AtomicU64,
    reset_pending_discards: AtomicU64,
    reset_pending_bytes: AtomicU64,
    raw_lag_events: AtomicU64,
    raw_lag_messages: AtomicU64,
    encoder_init_errors: AtomicU64,
    encoder_errors: AtomicU64,
    emitted_packets: AtomicU64,
    emitted_frames: AtomicU64,
    emitted_wire_bytes: AtomicU64,
    // Writer-owned; never touched by the packetizer.
    writer_stale_packets: AtomicU64,
    writer_stale_bytes: AtomicU64,
    writer_lag_events: AtomicU64,
    writer_lag_messages: AtomicU64,
}

/// Plain copy of [`Counters`] taken field by field (see the coherence note).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CountersSnapshot {
    /// Raw `Event::Data` messages discarded on arrival because their ingress
    /// instant was older than `MAX_AGE`, and their stereo s16le byte total.
    pub stale_raw_events: u64,
    pub stale_raw_bytes: u64,
    /// Partial or just-completed packets discarded because their *first*
    /// fragment's ingress age exceeded `MAX_AGE`, and their PCM byte total.
    pub stale_pending_discards: u64,
    pub stale_pending_bytes: u64,
    /// Non-empty partial packets discarded for any other reason: a stale raw
    /// event, `Begin`/`End`, raw lag or a request change, and their PCM bytes.
    pub reset_pending_discards: u64,
    pub reset_pending_bytes: u64,
    /// Raw broadcast lag notifications and the messages they reported skipped.
    pub raw_lag_events: u64,
    pub raw_lag_messages: u64,
    /// Encoder construction failures (the request stays unencoded).
    pub encoder_init_errors: u64,
    /// `Encoder::packet` failures on a complete, fresh packet.
    pub encoder_errors: u64,
    /// Packets handed to the broadcast queue, their frames and GLA1 wire bytes
    /// (header plus payload). Handing over is not delivery.
    pub emitted_packets: u64,
    pub emitted_frames: u64,
    pub emitted_wire_bytes: u64,
    /// Encoded packets a writer reported dropping for age, and their wire bytes.
    /// Only ever incremented through [`Counters::record_writer_stale_packet`].
    pub writer_stale_packets: u64,
    pub writer_stale_bytes: u64,
    /// Encoded broadcast lag notifications and messages the writer missed.
    pub writer_lag_events: u64,
    pub writer_lag_messages: u64,
}

impl Counters {
    pub fn snapshot(&self) -> CountersSnapshot {
        let load = |field: &AtomicU64| field.load(Ordering::Relaxed);
        CountersSnapshot {
            stale_raw_events: load(&self.stale_raw_events),
            stale_raw_bytes: load(&self.stale_raw_bytes),
            stale_pending_discards: load(&self.stale_pending_discards),
            stale_pending_bytes: load(&self.stale_pending_bytes),
            reset_pending_discards: load(&self.reset_pending_discards),
            reset_pending_bytes: load(&self.reset_pending_bytes),
            raw_lag_events: load(&self.raw_lag_events),
            raw_lag_messages: load(&self.raw_lag_messages),
            encoder_init_errors: load(&self.encoder_init_errors),
            encoder_errors: load(&self.encoder_errors),
            emitted_packets: load(&self.emitted_packets),
            emitted_frames: load(&self.emitted_frames),
            emitted_wire_bytes: load(&self.emitted_wire_bytes),
            writer_stale_packets: load(&self.writer_stale_packets),
            writer_stale_bytes: load(&self.writer_stale_bytes),
            writer_lag_events: load(&self.writer_lag_events),
            writer_lag_messages: load(&self.writer_lag_messages),
        }
    }

    /// Writer-side increment path: one encoded packet the session writer
    /// dropped because `received.elapsed() > MAX_AGE` at send time, with its
    /// GLA1 wire length. Separate from every packetizer-owned field; nothing
    /// in this module calls it.
    pub fn record_writer_stale_packet(&self, wire_bytes: usize) {
        self.writer_stale_packets.fetch_add(1, Ordering::Relaxed);
        self.writer_stale_bytes
            .fetch_add(wire_bytes as u64, Ordering::Relaxed);
    }

    /// One encoded broadcast lag notification and its skipped message count.
    /// This does not identify the skipped packets' generation, age or size.
    pub fn record_writer_lag(&self, skipped: u64) {
        self.writer_lag_events.fetch_add(1, Ordering::Relaxed);
        self.writer_lag_messages
            .fetch_add(skipped, Ordering::Relaxed);
    }

    fn stale_raw(&self, bytes: usize) {
        self.stale_raw_events.fetch_add(1, Ordering::Relaxed);
        self.stale_raw_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn stale_pending(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.stale_pending_discards.fetch_add(1, Ordering::Relaxed);
        self.stale_pending_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn reset_pending(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.reset_pending_discards.fetch_add(1, Ordering::Relaxed);
        self.reset_pending_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn raw_lag(&self, skipped: u64) {
        self.raw_lag_events.fetch_add(1, Ordering::Relaxed);
        self.raw_lag_messages.fetch_add(skipped, Ordering::Relaxed);
    }

    fn emitted(&self, frames: u32, wire_bytes: usize) {
        self.emitted_packets.fetch_add(1, Ordering::Relaxed);
        self.emitted_frames
            .fetch_add(u64::from(frames), Ordering::Relaxed);
        self.emitted_wire_bytes
            .fetch_add(wire_bytes as u64, Ordering::Relaxed);
    }
}

/// Explicit opt-in configuration for [`encode_with`]. The default is the
/// historical behavior (20 ms packets) with a fresh, unobserved counter set.
#[derive(Clone, Debug, Default)]
pub struct EncodeConfig {
    pub packet_duration: PacketDuration,
    pub counters: Arc<Counters>,
}

impl EncodeConfig {
    pub fn new(packet_duration: PacketDuration) -> Self {
        Self {
            packet_duration,
            counters: Arc::default(),
        }
    }
}

struct Encoder {
    codec: Codec,
    /// Frozen at construction: header frame count and exact PCM input length.
    frames: u32,
    pcm_bytes: usize,
    opus: Option<opus::Encoder>,
    adpcm: super::audio_adpcm::Encoder,
    pending: Vec<u8>,
    pending_received: Option<Instant>,
    sequence: u32,
}

impl Encoder {
    /// Historical constructor (20 ms policy), kept for the original tests.
    #[cfg(test)]
    fn new(codec: Codec) -> Result<Self> {
        Self::with_duration(codec, PacketDuration::DEFAULT)
    }

    fn with_duration(codec: Codec, duration: PacketDuration) -> Result<Self> {
        let opus = if codec == Codec::Opus {
            let mut encoder = opus::Encoder::new(
                SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::Audio,
            )?;
            encoder.set_bitrate(opus::Bitrate::Bits(96_000))?;
            encoder.set_complexity(4)?;
            Some(encoder)
        } else {
            None
        };
        let pcm_bytes = duration.pcm_bytes();
        Ok(Self {
            codec,
            frames: duration.frames(),
            pcm_bytes,
            opus,
            adpcm: super::audio_adpcm::Encoder::default(),
            pending: Vec::with_capacity(pcm_bytes),
            pending_received: None,
            sequence: 0,
        })
    }

    /// Drops any partial packet and returns how many PCM bytes it held.
    fn clear_pending(&mut self) -> usize {
        let dropped = self.pending.len();
        self.pending.clear();
        self.pending_received = None;
        dropped
    }

    fn packet(&mut self, pcm: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            pcm.len() == self.pcm_bytes,
            "audio packet needs {} ms ({} bytes) of stereo s16le, got {} bytes",
            self.frames / (SAMPLE_RATE / 1000),
            self.pcm_bytes,
            pcm.len()
        );
        let payload = if self.codec != Codec::Pcm {
            let samples: Vec<i16> = pcm
                .as_chunks::<2>()
                .0
                .iter()
                .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
                .collect();
            if self.codec == Codec::ImaAdpcm {
                self.adpcm.encode(&samples)?
            } else {
                self.opus
                    .as_mut()
                    .expect("Opus encoder")
                    .encode_vec(&samples, 1275)?
            }
        } else {
            pcm.to_vec()
        };
        let mut packet = Vec::with_capacity(20 + payload.len());
        packet.extend_from_slice(b"GLA1");
        let wire_codec = match self.codec {
            Codec::Pcm => 0,
            Codec::Opus => 1,
            Codec::ImaAdpcm => 2,
        };
        packet.extend_from_slice(&[wire_codec, CHANNELS, 0, 0]);
        packet.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        packet.extend_from_slice(&self.sequence.to_le_bytes());
        packet.extend_from_slice(&self.frames.to_le_bytes());
        packet.extend_from_slice(&payload);
        self.sequence = self.sequence.wrapping_add(1);
        Ok(packet)
    }
}

/// Historical entry point: 20 ms packets, counters unobserved.
/// A lagging source or writer drops sound; neither can block capture or input.
pub async fn encode(
    raw: broadcast::Receiver<Event>,
    requests: watch::Receiver<Request>,
    packets: broadcast::Sender<Packet>,
) {
    encode_with(raw, requests, packets, EncodeConfig::default()).await;
}

/// Opt-in entry point. Identical to [`encode`] except for the frozen packet
/// duration and the counter set it reports into. `MAX_AGE` still bounds the
/// first fragment's ingress age regardless of packet duration.
pub async fn encode_with(
    mut raw: broadcast::Receiver<Event>,
    mut requests: watch::Receiver<Request>,
    packets: broadcast::Sender<Packet>,
    config: EncodeConfig,
) {
    let EncodeConfig {
        packet_duration,
        counters,
    } = config;
    let mut current = Request::default();
    let mut encoder: Option<Encoder> = None;
    loop {
        tokio::select! {
            biased;
            changed = requests.changed() => {
                if changed.is_err() { break; }
                current = *requests.borrow_and_update();
                raw = raw.resubscribe();
                if let Some(encoder) = &mut encoder { counters.reset_pending(encoder.clear_pending()); }
                encoder = current.codec.and_then(|codec| match Encoder::with_duration(codec, packet_duration) {
                    Ok(encoder) => Some(encoder),
                    Err(error) => {
                        counters.encoder_init_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("audio encoder unavailable: {error}");
                        None
                    }
                });
            }
            event = raw.recv() => {
                match event {
                    Ok(Event::Data { bytes, received }) => {
                        let Some(encoder) = &mut encoder else { continue; };
                        if received.elapsed() > MAX_AGE {
                            counters.stale_raw(bytes.len());
                            counters.reset_pending(encoder.clear_pending());
                            continue;
                        }
                        // A fresh fragment must not renew the age of a partial packet.
                        if encoder.pending_received.is_some_and(|first| first.elapsed() > MAX_AGE) {
                            counters.stale_pending(encoder.clear_pending());
                        }
                        // Retain at most one partial packet even for a maximal RFB chunk.
                        let mut rest = bytes.as_ref();
                        while !rest.is_empty() {
                            if encoder.pending.is_empty() { encoder.pending_received = Some(received); }
                            let count = rest.len().min(encoder.pcm_bytes - encoder.pending.len());
                            encoder.pending.extend_from_slice(&rest[..count]);
                            rest = &rest[count..];
                            if encoder.pending.len() == encoder.pcm_bytes {
                                let received = encoder.pending_received.take().expect("partial audio ingress time");
                                let pcm = std::mem::take(&mut encoder.pending);
                                if received.elapsed() <= MAX_AGE {
                                    match encoder.packet(&pcm) {
                                        Ok(bytes) => {
                                            counters.emitted(encoder.frames, bytes.len());
                                            let _ = packets.send(Packet { generation: current.generation, received, bytes: bytes.into() });
                                        }
                                        Err(error) => {
                                            counters.encoder_errors.fetch_add(1, Ordering::Relaxed);
                                            tracing::warn!("audio encoding failed: {error}");
                                        }
                                    }
                                } else {
                                    counters.stale_pending(pcm.len());
                                }
                                encoder.pending = pcm;
                                encoder.pending.clear();
                            }
                        }
                    }
                    Ok(Event::Begin | Event::End) => {
                        if let Some(encoder) = &mut encoder { counters.reset_pending(encoder.clear_pending()); }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        counters.raw_lag(skipped);
                        if let Some(encoder) = &mut encoder { counters.reset_pending(encoder.clear_pending()); }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(start: usize) -> Vec<u8> {
        (start..start + FRAMES as usize)
            .flat_map(|i| {
                [440.0, 880.0].into_iter().flat_map(move |frequency| {
                    let sample = (12_000.0
                        * (i as f64 * frequency * std::f64::consts::TAU / f64::from(SAMPLE_RATE))
                            .sin()) as i16;
                    sample.to_le_bytes()
                })
            })
            .collect()
    }

    #[test]
    fn opus_roundtrip_preserves_stereo_at_bounded_bitrate() {
        let mut encoder = Encoder::new(Codec::Opus).unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut bytes = 0;
        let mut energy = [0f64; 2];
        let started = Instant::now();
        for sequence in 0..100usize {
            let pcm = tone(sequence * FRAMES as usize);
            let packet = encoder.packet(&pcm).unwrap();
            assert_eq!(&packet[..12], &[71, 76, 65, 49, 1, 2, 0, 0, 128, 187, 0, 0]);
            assert_eq!(
                u32::from_le_bytes(packet[12..16].try_into().unwrap()),
                sequence as u32
            );
            assert_eq!(
                u32::from_le_bytes(packet[16..20].try_into().unwrap()),
                FRAMES
            );
            let mut samples = [0i16; FRAMES as usize * 2];
            assert_eq!(
                decoder.decode(&packet[20..], &mut samples, false).unwrap(),
                FRAMES as usize
            );
            for frame in samples.as_chunks::<2>().0 {
                for channel in 0..2 {
                    energy[channel] += f64::from(frame[channel]).powi(2);
                }
            }
            bytes += packet.len();
        }
        assert!(
            bytes < 35_000,
            "2 seconds including headers used {bytes} bytes"
        );
        for channel in energy {
            assert!((channel / 48_000.0).sqrt() > 5_000.0);
        }
        eprintln!(
            "Opus: {} kbit/s including GLA1 headers; 2 s encode/decode took {:?}",
            bytes * 4 / 1000,
            started.elapsed()
        );
    }

    #[test]
    fn adpcm_is_advertised_and_framed_only_when_requested() {
        assert_eq!(
            Status::default().message()["codecs"],
            json!(["opus", "ima-adpcm", "pcm"])
        );
        let (session, requests, _) = channel();
        request(
            &session.request,
            json!({"enabled":true,"codec":"ima-adpcm"}),
        )
        .unwrap();
        assert_eq!(requests.borrow().codec, Some(Codec::ImaAdpcm));
        assert_eq!(requests.borrow().generation, 1);
        assert!(
            request(
                &session.request,
                json!({"enabled":true,"codec":"ima-adpcm-unknown"})
            )
            .is_err()
        );
        let mut encoder = Encoder::new(Codec::ImaAdpcm).unwrap();
        let pcm = tone(0);
        let packet = encoder.packet(&pcm).unwrap();
        assert_eq!(packet.len(), 987);
        assert_eq!(packet[4], 2);
        assert_eq!(u32::from_le_bytes(packet[16..20].try_into().unwrap()), 960);
        assert_eq!(
            super::super::audio_adpcm::decode(&packet[20..], 960)
                .unwrap()
                .len(),
            1920
        );
    }

    #[tokio::test]
    async fn fragmented_pcm_is_exact_and_audio_is_only_encoded_on_request() {
        let (session, requests, _) = channel();
        let (raw, receiver) = broadcast::channel(8);
        let mut packets = session.packets.subscribe();
        let task = tokio::spawn(encode(receiver, requests, session.packets.clone()));
        let pcm = tone(0);
        raw.send(Event::Data {
            bytes: pcm.clone().into(),
            received: Instant::now(),
        })
        .unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            packets.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        request(&session.request, json!({"enabled":true, "codec":"pcm"})).unwrap();
        tokio::task::yield_now().await;
        for chunk in pcm.chunks(1024) {
            raw.send(Event::Data {
                bytes: chunk.into(),
                received: Instant::now(),
            })
            .unwrap();
        }
        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(packet.generation, 1);
        assert_eq!(&packet.bytes[20..], pcm);
        request(&session.request, json!({"enabled":false})).unwrap();
        tokio::task::yield_now().await;
        raw.send(Event::Data {
            bytes: pcm.into(),
            received: Instant::now(),
        })
        .unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            packets.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop(raw);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn old_sound_and_partial_samples_do_not_cross_mute_or_overload() {
        let (session, requests, _) = channel();
        let (raw, receiver) = broadcast::channel(8);
        let mut packets = session.packets.subscribe();
        let task = tokio::spawn(encode(receiver, requests, session.packets.clone()));
        request(&session.request, json!({"enabled":true, "codec":"pcm"})).unwrap();
        tokio::task::yield_now().await;
        raw.send(Event::Data {
            bytes: vec![255; PCM_BYTES / 2].into(),
            received: Instant::now(),
        })
        .unwrap();
        tokio::task::yield_now().await;
        request(&session.request, json!({"enabled":false})).unwrap();
        request(&session.request, json!({"enabled":true, "codec":"pcm"})).unwrap();
        tokio::task::yield_now().await;
        raw.send(Event::Data {
            bytes: vec![123; PCM_BYTES].into(),
            received: Instant::now() - Duration::from_secs(1),
        })
        .unwrap();
        raw.send(Event::Data {
            bytes: vec![0; PCM_BYTES].into(),
            received: Instant::now(),
        })
        .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(packet.generation, 3);
        assert!(packet.bytes[20..].iter().all(|byte| *byte == 0));
        assert_eq!(
            u32::from_le_bytes(packet.bytes[12..16].try_into().unwrap()),
            0
        );
        drop(raw);
        task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fragmented_packet_retains_its_first_chunk_ingress_time() {
        let (session, requests, _) = channel();
        let (raw, receiver) = broadcast::channel(8);
        let mut packets = session.packets.subscribe();
        let task = tokio::spawn(encode(receiver, requests, session.packets.clone()));
        request(&session.request, json!({"enabled":true, "codec":"pcm"})).unwrap();
        tokio::task::yield_now().await;
        let first = Instant::now();
        raw.send(Event::Data {
            bytes: vec![17; PCM_BYTES / 2].into(),
            received: first,
        })
        .unwrap();
        tokio::task::yield_now().await;
        let second = Instant::now();
        assert!(first < second);
        raw.send(Event::Data {
            bytes: vec![34; PCM_BYTES / 2].into(),
            received: second,
        })
        .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            packet.received, first,
            "The final fragment must not make older samples appear newly received"
        );
        assert!(
            packet.bytes[20..20 + PCM_BYTES / 2]
                .iter()
                .all(|b| *b == 17)
        );
        assert!(packet.bytes[20 + PCM_BYTES / 2..].iter().all(|b| *b == 34));
        drop(raw);
        task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_partial_packet_does_not_mix_with_fresh_sound_after_a_pause() {
        let (session, requests, _) = channel();
        let (raw, receiver) = broadcast::channel(8);
        let mut packets = session.packets.subscribe();
        let task = tokio::spawn(encode(receiver, requests, session.packets.clone()));
        request(&session.request, json!({"enabled":true, "codec":"pcm"})).unwrap();
        tokio::task::yield_now().await;
        raw.send(Event::Data {
            bytes: vec![17; PCM_BYTES / 2].into(),
            received: Instant::now(),
        })
        .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            raw.len(),
            0,
            "The partial chunk must be consumed before the pause"
        );
        tokio::time::sleep(MAX_AGE + Duration::from_millis(20)).await;
        let fresh = vec![34; PCM_BYTES];
        let received = Instant::now();
        raw.send(Event::Data {
            bytes: fresh.clone().into(),
            received,
        })
        .unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            packet.bytes[20..] == fresh,
            "Expired partial audio was mixed into the fresh packet"
        );
        assert_eq!(packet.received, received);
        drop(raw);
        task.await.unwrap();
    }

    // ---- Packet-duration policy, per-encoder sizing, counters (new) ----

    const GLA1_OPUS_HEADER: [u8; 12] = [71, 76, 65, 49, 1, 2, 0, 0, 128, 187, 0, 0];

    fn tone_frames(start: usize, frames: usize) -> Vec<u8> {
        (start..start + frames)
            .flat_map(|i| {
                [440.0, 880.0].into_iter().flat_map(move |frequency| {
                    let sample = (12_000.0
                        * (i as f64 * frequency * std::f64::consts::TAU / f64::from(SAMPLE_RATE))
                            .sin()) as i16;
                    sample.to_le_bytes()
                })
            })
            .collect()
    }

    fn header_u32(packet: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(packet[offset..offset + 4].try_into().unwrap())
    }

    struct Pipeline {
        session: Session,
        raw: broadcast::Sender<Event>,
        packets: broadcast::Receiver<Packet>,
        task: tokio::task::JoinHandle<()>,
        counters: Arc<Counters>,
    }

    impl Pipeline {
        fn new(duration: PacketDuration, raw_capacity: usize) -> Self {
            let (session, requests, _) = channel();
            let (raw, receiver) = broadcast::channel(raw_capacity);
            let packets = session.packets.subscribe();
            let config = EncodeConfig::new(duration);
            let counters = config.counters.clone();
            let task = tokio::spawn(encode_with(
                receiver,
                requests,
                session.packets.clone(),
                config,
            ));
            Self {
                session,
                raw,
                packets,
                task,
                counters,
            }
        }

        async fn enable(&self, codec: &str) {
            request(
                &self.session.request,
                json!({"enabled":true, "codec":codec}),
            )
            .unwrap();
            tokio::task::yield_now().await;
        }

        async fn disable(&self) {
            request(&self.session.request, json!({"enabled":false})).unwrap();
            tokio::task::yield_now().await;
        }

        fn send(&self, bytes: Vec<u8>, received: Instant) {
            self.raw
                .send(Event::Data {
                    bytes: bytes.into(),
                    received,
                })
                .unwrap();
        }

        async fn next(&mut self) -> Packet {
            tokio::time::timeout(Duration::from_secs(1), self.packets.recv())
                .await
                .expect("audio packet within one second")
                .unwrap()
        }

        async fn none(&mut self) {
            tokio::task::yield_now().await;
            assert!(
                matches!(
                    self.packets.try_recv(),
                    Err(broadcast::error::TryRecvError::Empty)
                ),
                "unexpected audio packet"
            );
        }

        async fn finish(self) -> CountersSnapshot {
            drop(self.raw);
            self.task.await.unwrap();
            self.counters.snapshot()
        }
    }

    #[test]
    fn packet_duration_accepts_only_canonical_20_and_40() {
        assert_eq!(PacketDuration::default(), PacketDuration::Ms20);
        assert_eq!(PacketDuration::DEFAULT, PacketDuration::Ms20);
        assert_eq!(
            EncodeConfig::default().packet_duration,
            PacketDuration::Ms20
        );
        assert_eq!(FRAMES, 960);
        assert_eq!(PCM_BYTES, 3840);
        assert_eq!(MAX_AGE, Duration::from_millis(100));
        for (duration, millis, frames, pcm_bytes) in [
            (PacketDuration::Ms20, 20, 960, 3840),
            (PacketDuration::Ms40, 40, 1920, 7680),
        ] {
            assert_eq!(duration.millis(), millis);
            assert_eq!(duration.frames(), frames);
            assert_eq!(duration.pcm_bytes(), pcm_bytes);
            assert_eq!(PacketDuration::from_millis(millis).unwrap(), duration);
            assert_eq!(
                millis.to_string().parse::<PacketDuration>().unwrap(),
                duration
            );
            assert_eq!(
                PacketDuration::from_str(&millis.to_string()).unwrap(),
                duration
            );
        }
        for invalid in [
            0,
            1,
            2,
            4,
            10,
            19,
            21,
            30,
            39,
            41,
            60,
            80,
            100,
            200,
            400,
            u32::MAX,
        ] {
            let error = PacketDuration::from_millis(invalid).unwrap_err();
            assert!(error.to_string().contains("20 or 40"), "{invalid}: {error}");
        }
        let invalid = [
            "", " ", "020", "040", " 20", "20 ", "+20", "-20", "20ms", "40.0",
        ]
        .into_iter()
        .chain(["2e1", "0x14", "20\n", "30", "60", "twenty", "Ms20"]);
        for invalid in invalid {
            assert!(
                invalid.parse::<PacketDuration>().is_err(),
                "{invalid:?} must not parse"
            );
        }
    }

    #[test]
    fn forty_ms_encoder_freezes_header_and_payload_sizes_per_codec() {
        let pcm40 = tone_frames(0, 1920);
        let pcm20 = tone(0);

        let mut pcm = Encoder::with_duration(Codec::Pcm, PacketDuration::Ms40).unwrap();
        let packet = pcm.packet(&pcm40).unwrap();
        assert_eq!(packet.len(), 20 + 7680);
        assert_eq!(&packet[..4], b"GLA1");
        assert_eq!(&packet[4..8], &[0, 2, 0, 0]);
        assert_eq!(header_u32(&packet, 8), SAMPLE_RATE);
        assert_eq!(header_u32(&packet, 12), 0);
        assert_eq!(header_u32(&packet, 16), 1920);
        assert_eq!(&packet[20..], &pcm40[..]);
        assert_eq!(header_u32(&pcm.packet(&pcm40).unwrap(), 12), 1);

        let mut adpcm = Encoder::with_duration(Codec::ImaAdpcm, PacketDuration::Ms40).unwrap();
        let packet = adpcm.packet(&pcm40).unwrap();
        assert_eq!(packet.len(), 20 + 1927);
        assert_eq!(packet[4], 2);
        assert_eq!(header_u32(&packet, 16), 1920);
        assert_eq!(
            super::super::audio_adpcm::decode(&packet[20..], 1920)
                .unwrap()
                .len(),
            3840
        );

        let mut opus_encoder = Encoder::with_duration(Codec::Opus, PacketDuration::Ms40).unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let packet = opus_encoder.packet(&pcm40).unwrap();
        assert_eq!(&packet[..12], &GLA1_OPUS_HEADER);
        assert_eq!(header_u32(&packet, 16), 1920);
        assert!((21..=20 + 1275).contains(&packet.len()));
        let mut samples = [0i16; 1920 * 2];
        assert_eq!(
            decoder.decode(&packet[20..], &mut samples, false).unwrap(),
            1920
        );

        // Sizing is frozen per encoder: neither accepts the other duration.
        for encoder in [&mut pcm, &mut adpcm, &mut opus_encoder] {
            let error = encoder.packet(&pcm20).unwrap_err().to_string();
            assert!(error.contains("40 ms") && error.contains("7680"), "{error}");
        }
        for codec in [Codec::Pcm, Codec::ImaAdpcm, Codec::Opus] {
            let mut twenty = Encoder::with_duration(codec, PacketDuration::Ms20).unwrap();
            let error = twenty.packet(&pcm40).unwrap_err().to_string();
            assert!(error.contains("20 ms") && error.contains("3840"), "{error}");
            assert!(twenty.packet(&[]).is_err());
            let packet = twenty.packet(&pcm20).unwrap();
            assert_eq!(header_u32(&packet, 16), FRAMES);
            let mut historical = Encoder::new(codec).unwrap();
            assert_eq!(historical.packet(&pcm20).unwrap(), packet);
        }
    }

    #[test]
    fn forty_ms_opus_roundtrip_preserves_stereo_at_bounded_bitrate() {
        let mut encoder = Encoder::with_duration(Codec::Opus, PacketDuration::Ms40).unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut bytes = 0;
        let mut energy = [0f64; 2];
        for sequence in 0..50usize {
            let pcm = tone_frames(sequence * 1920, 1920);
            let packet = encoder.packet(&pcm).unwrap();
            assert_eq!(&packet[..12], &GLA1_OPUS_HEADER);
            assert_eq!(header_u32(&packet, 12), sequence as u32);
            assert_eq!(header_u32(&packet, 16), 1920);
            let mut samples = [0i16; 1920 * 2];
            assert_eq!(
                decoder.decode(&packet[20..], &mut samples, false).unwrap(),
                1920
            );
            for frame in samples.as_chunks::<2>().0 {
                for channel in 0..2 {
                    energy[channel] += f64::from(frame[channel]).powi(2);
                }
            }
            bytes += packet.len();
        }
        assert!(
            bytes < 35_000,
            "2 seconds of 40 ms packets including headers used {bytes} bytes"
        );
        for channel in energy {
            assert!((channel / 96_000.0).sqrt() > 5_000.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn two_20ms_fragments_assemble_into_one_40ms_packet_keeping_first_ingress() {
        let mut pipeline = Pipeline::new(PacketDuration::Ms40, 8);
        pipeline.enable("pcm").await;
        let first = Instant::now();
        pipeline.send(vec![17; 3840], first);
        pipeline.none().await;
        let second = Instant::now();
        assert!(first < second);
        pipeline.send(vec![34; 3840], second);
        let packet = pipeline.next().await;
        assert_eq!(packet.generation, 1);
        assert_eq!(packet.received, first);
        assert_eq!(packet.bytes.len(), 20 + 7680);
        assert_eq!(&packet.bytes[4..8], &[0, 2, 0, 0]);
        assert_eq!(header_u32(&packet.bytes, 12), 0);
        assert_eq!(header_u32(&packet.bytes, 16), 1920);
        assert!(packet.bytes[20..20 + 3840].iter().all(|b| *b == 17));
        assert!(packet.bytes[20 + 3840..].iter().all(|b| *b == 34));
        pipeline.none().await;
        let counters = pipeline.finish().await;
        assert_eq!(
            counters,
            CountersSnapshot {
                emitted_packets: 1,
                emitted_frames: 1920,
                emitted_wire_bytes: 20 + 7680,
                ..CountersSnapshot::default()
            }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forty_ms_pipeline_emits_opus_and_adpcm_that_decode_to_1920_frames() {
        let mut pipeline = Pipeline::new(PacketDuration::Ms40, 8);
        pipeline.enable("opus").await;
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        for sequence in 0..3u32 {
            pipeline.send(tone_frames(sequence as usize * 1920, 1920), Instant::now());
            let packet = pipeline.next().await;
            assert_eq!(&packet.bytes[..12], &GLA1_OPUS_HEADER);
            assert_eq!(header_u32(&packet.bytes, 12), sequence);
            assert_eq!(header_u32(&packet.bytes, 16), 1920);
            let mut samples = [0i16; 1920 * 2];
            assert_eq!(
                decoder
                    .decode(&packet.bytes[20..], &mut samples, false)
                    .unwrap(),
                1920
            );
        }
        pipeline.enable("ima-adpcm").await;
        pipeline.send(tone_frames(0, 1920), Instant::now());
        let packet = pipeline.next().await;
        assert_eq!(packet.generation, 2);
        assert_eq!(packet.bytes.len(), 20 + 1927);
        assert_eq!(packet.bytes[4], 2);
        assert_eq!(header_u32(&packet.bytes, 12), 0);
        assert_eq!(header_u32(&packet.bytes, 16), 1920);
        assert_eq!(
            super::super::audio_adpcm::decode(&packet.bytes[20..], 1920)
                .unwrap()
                .len(),
            3840
        );
        let counters = pipeline.finish().await;
        assert_eq!(counters.emitted_packets, 4);
        assert_eq!(counters.emitted_frames, 4 * 1920);
        assert_eq!(counters.encoder_errors, 0);
        assert_eq!(counters.encoder_init_errors, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forty_ms_pipeline_splits_a_large_chunk_into_whole_packets_and_one_partial() {
        let mut pipeline = Pipeline::new(PacketDuration::Ms40, 8);
        pipeline.enable("pcm").await;
        // Two whole 40 ms packets plus half of a third in one raw event.
        let chunk: Vec<u8> = (0..7680 * 2 + 3840).map(|i| (i / 7680) as u8 + 1).collect();
        let ingress = Instant::now();
        pipeline.send(chunk, ingress);
        for sequence in 0..2u32 {
            let packet = pipeline.next().await;
            assert_eq!(packet.received, ingress);
            assert_eq!(header_u32(&packet.bytes, 12), sequence);
            assert_eq!(header_u32(&packet.bytes, 16), 1920);
            assert!(packet.bytes[20..].iter().all(|b| *b == sequence as u8 + 1));
        }
        pipeline.none().await;
        let later = Instant::now();
        assert!(ingress < later);
        pipeline.send(vec![4; 3840], later);
        let packet = pipeline.next().await;
        assert_eq!(
            packet.received, ingress,
            "the partial keeps its own ingress"
        );
        assert_eq!(header_u32(&packet.bytes, 12), 2);
        assert!(packet.bytes[20..20 + 3840].iter().all(|b| *b == 3));
        assert!(packet.bytes[20 + 3840..].iter().all(|b| *b == 4));
        let counters = pipeline.finish().await;
        assert_eq!(counters.emitted_packets, 3);
        assert_eq!(counters.emitted_frames, 3 * 1920);
        assert_eq!(counters.emitted_wire_bytes, 3 * (20 + 7680));
        assert_eq!(counters.stale_pending_discards, 0);
        assert_eq!(counters.reset_pending_discards, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_fragment_age_is_bounded_by_the_fixed_100ms_budget_for_both_durations() {
        assert_eq!(MAX_AGE, Duration::from_millis(100));
        for duration in [PacketDuration::Ms20, PacketDuration::Ms40] {
            let half = duration.pcm_bytes() / 2;
            let mut pipeline = Pipeline::new(duration, 8);
            pipeline.enable("pcm").await;
            pipeline.send(vec![17; half], Instant::now());
            pipeline.none().await;
            assert_eq!(
                pipeline.raw.len(),
                0,
                "the partial must be consumed before the pause"
            );
            tokio::time::sleep(MAX_AGE + Duration::from_millis(20)).await;
            // A fresh last fragment must not renew the expired partial packet.
            let fresh = Instant::now();
            pipeline.send(vec![34; half], fresh);
            pipeline.none().await;
            pipeline.send(vec![34; half], Instant::now());
            let packet = pipeline.next().await;
            assert_eq!(packet.received, fresh, "{duration:?}");
            assert_eq!(packet.bytes.len(), 20 + duration.pcm_bytes());
            assert_eq!(header_u32(&packet.bytes, 16), duration.frames());
            assert!(
                packet.bytes[20..].iter().all(|b| *b == 34),
                "{duration:?}: expired partial audio was mixed into the fresh packet"
            );
            let counters = pipeline.finish().await;
            assert_eq!(
                counters,
                CountersSnapshot {
                    stale_pending_discards: 1,
                    stale_pending_bytes: half as u64,
                    emitted_packets: 1,
                    emitted_frames: u64::from(duration.frames()),
                    emitted_wire_bytes: 20 + duration.pcm_bytes() as u64,
                    ..CountersSnapshot::default()
                },
                "{duration:?}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pause_reset_mute_and_stale_sound_discard_partial_40ms_packets() {
        let mut pipeline = Pipeline::new(PacketDuration::Ms40, 8);
        pipeline.enable("pcm").await;

        // Begin: the partial is reset, the next whole packet is exact.
        pipeline.send(vec![17; 3840], Instant::now());
        pipeline.raw.send(Event::Begin).unwrap();
        pipeline.send(vec![34; 7680], Instant::now());
        let packet = pipeline.next().await;
        assert_eq!(packet.generation, 1);
        assert_eq!(header_u32(&packet.bytes, 12), 0);
        assert!(packet.bytes[20..].iter().all(|b| *b == 34));
        assert_eq!(pipeline.counters.snapshot().reset_pending_discards, 1);

        // End: same reset, sequence keeps counting inside the generation.
        pipeline.send(vec![17; 3840], Instant::now());
        pipeline.raw.send(Event::End).unwrap();
        pipeline.send(vec![35; 7680], Instant::now());
        let packet = pipeline.next().await;
        assert_eq!(header_u32(&packet.bytes, 12), 1);
        assert!(packet.bytes[20..].iter().all(|b| *b == 35));
        assert_eq!(pipeline.counters.snapshot().reset_pending_discards, 2);

        // Mute then unmute: a new generation starts at sequence 0 without old bytes.
        pipeline.send(vec![17; 3840], Instant::now());
        pipeline.none().await;
        pipeline.disable().await;
        pipeline.send(vec![18; 7680], Instant::now());
        pipeline.none().await;
        pipeline.enable("pcm").await;
        pipeline.send(vec![36; 7680], Instant::now());
        let packet = pipeline.next().await;
        assert_eq!(packet.generation, 3);
        assert_eq!(header_u32(&packet.bytes, 12), 0);
        assert!(packet.bytes[20..].iter().all(|b| *b == 36));
        assert_eq!(pipeline.counters.snapshot().reset_pending_discards, 3);

        // A stale raw event discards itself and the partial it would have joined.
        pipeline.send(vec![17; 3840], Instant::now());
        pipeline.none().await;
        pipeline.send(vec![123; 7680], Instant::now() - Duration::from_secs(1));
        pipeline.none().await;
        pipeline.send(vec![37; 7680], Instant::now());
        let packet = pipeline.next().await;
        assert_eq!(header_u32(&packet.bytes, 12), 1);
        assert!(packet.bytes[20..].iter().all(|b| *b == 37));

        let counters = pipeline.finish().await;
        assert_eq!(
            counters,
            CountersSnapshot {
                stale_raw_events: 1,
                stale_raw_bytes: 7680,
                reset_pending_discards: 4,
                reset_pending_bytes: 4 * 3840,
                emitted_packets: 4,
                emitted_frames: 4 * 1920,
                emitted_wire_bytes: 4 * (20 + 7680),
                ..CountersSnapshot::default()
            }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raw_lag_clears_partial_sound_and_is_counted() {
        let mut pipeline = Pipeline::new(PacketDuration::Ms20, 2);
        pipeline.enable("pcm").await;
        pipeline.send(vec![17; 1920], Instant::now());
        pipeline.none().await;
        // Three whole packets into a two-slot queue: the first is lost to lag.
        for value in [1, 2, 3] {
            pipeline.send(vec![value; 3840], Instant::now());
        }
        for (sequence, value) in [(0, 2), (1, 3)] {
            let packet = pipeline.next().await;
            assert_eq!(header_u32(&packet.bytes, 12), sequence);
            assert_eq!(header_u32(&packet.bytes, 16), 960);
            assert!(packet.bytes[20..].iter().all(|b| *b == value));
        }
        pipeline.none().await;
        let counters = pipeline.finish().await;
        assert_eq!(
            counters,
            CountersSnapshot {
                reset_pending_discards: 1,
                reset_pending_bytes: 1920,
                raw_lag_events: 1,
                raw_lag_messages: 1,
                emitted_packets: 2,
                emitted_frames: 1920,
                emitted_wire_bytes: 2 * (20 + 3840),
                ..CountersSnapshot::default()
            }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disabled_raw_data_is_unencoded_and_excluded_from_discard_counters() {
        for duration in [PacketDuration::Ms20, PacketDuration::Ms40] {
            let mut pipeline = Pipeline::new(duration, 8);
            pipeline.send(
                vec![17; 3840],
                Instant::now() - MAX_AGE - Duration::from_millis(1),
            );
            pipeline.send(vec![34; 3840], Instant::now());
            pipeline.none().await;
            assert_eq!(pipeline.finish().await, CountersSnapshot::default());
        }
    }

    #[test]
    fn counters_start_at_zero_and_the_writer_stale_path_is_separate() {
        let counters = Counters::default();
        assert_eq!(counters.snapshot(), CountersSnapshot::default());
        counters.record_writer_stale_packet(1300);
        counters.record_writer_stale_packet(260);
        counters.record_writer_lag(3);
        counters.record_writer_lag(2);
        assert_eq!(
            counters.snapshot(),
            CountersSnapshot {
                writer_stale_packets: 2,
                writer_stale_bytes: 1560,
                writer_lag_events: 2,
                writer_lag_messages: 5,
                ..CountersSnapshot::default()
            }
        );
        // Packetizer-side helpers ignore empty discards and never touch writer fields.
        counters.reset_pending(0);
        counters.stale_pending(0);
        assert_eq!(counters.snapshot().reset_pending_discards, 0);
        assert_eq!(counters.snapshot().stale_pending_discards, 0);
        counters.stale_raw(0);
        assert_eq!(counters.snapshot().stale_raw_events, 1);
        assert_eq!(counters.snapshot().writer_stale_packets, 2);

        let config = EncodeConfig::new(PacketDuration::Ms40);
        assert_eq!(config.packet_duration, PacketDuration::Ms40);
        assert!(Arc::ptr_eq(&config.counters, &config.clone().counters));
        assert_eq!(config.counters.snapshot(), CountersSnapshot::default());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_encode_matches_explicit_20ms_config_byte_for_byte() {
        let (session, requests, _) = channel();
        let (raw, receiver) = broadcast::channel(8);
        let mut historical = session.packets.subscribe();
        let task = tokio::spawn(encode(receiver, requests, session.packets.clone()));
        request(&session.request, json!({"enabled":true, "codec":"opus"})).unwrap();
        tokio::task::yield_now().await;
        let mut explicit = Pipeline::new(PacketDuration::Ms20, 8);
        explicit.enable("opus").await;
        for sequence in 0..6usize {
            let pcm = tone(sequence * FRAMES as usize);
            let received = Instant::now();
            raw.send(Event::Data {
                bytes: pcm.clone().into(),
                received,
            })
            .unwrap();
            explicit.send(pcm, received);
            let expected = tokio::time::timeout(Duration::from_secs(1), historical.recv())
                .await
                .unwrap()
                .unwrap();
            let actual = explicit.next().await;
            assert_eq!(actual.bytes, expected.bytes, "packet {sequence}");
            assert_eq!(actual.received, expected.received);
            assert_eq!(actual.generation, expected.generation);
            assert_eq!(header_u32(&actual.bytes, 16), 960);
        }
        drop(raw);
        task.await.unwrap();
        let counters = explicit.finish().await;
        assert_eq!(counters.emitted_packets, 6);
        assert_eq!(counters.emitted_frames, 6 * 960);
        assert!(counters.emitted_wire_bytes > 6 * 20);
    }
}
