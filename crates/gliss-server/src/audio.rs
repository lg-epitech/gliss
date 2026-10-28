//! On-demand VM sound, independent of the video ACK window.
//! Both raw and encoded queues discard stale sound instead of delaying the desktop.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use gliss_rfb::audio::{CHANNELS, Event, SAMPLE_RATE};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

pub const FRAMES: u32 = 960;
pub const MAX_AGE: Duration = Duration::from_millis(100);
const PCM_BYTES: usize = FRAMES as usize * CHANNELS as usize * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Pcm,
    Opus,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::Pcm => "pcm",
            Self::Opus => "opus",
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
        let mut value =
            json!({"type":"audio_status", "available":self.available, "enabled":self.enabled});
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

struct Encoder {
    codec: Codec,
    opus: Option<opus::Encoder>,
    pending: Vec<u8>,
    pending_received: Option<Instant>,
    sequence: u32,
}

impl Encoder {
    fn new(codec: Codec) -> Result<Self> {
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
        Ok(Self {
            codec,
            opus,
            pending: Vec::with_capacity(PCM_BYTES),
            pending_received: None,
            sequence: 0,
        })
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_received = None;
    }

    fn packet(&mut self, pcm: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            pcm.len() == PCM_BYTES,
            "audio packet needs 20 ms of stereo s16le"
        );
        let payload = if let Some(encoder) = &mut self.opus {
            let samples: Vec<i16> = pcm
                .as_chunks::<2>()
                .0
                .iter()
                .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
                .collect();
            encoder.encode_vec(&samples, 1275)?
        } else {
            pcm.to_vec()
        };
        let mut packet = Vec::with_capacity(20 + payload.len());
        packet.extend_from_slice(b"GLA1");
        packet.extend_from_slice(&[u8::from(self.codec == Codec::Opus), CHANNELS, 0, 0]);
        packet.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        packet.extend_from_slice(&self.sequence.to_le_bytes());
        packet.extend_from_slice(&FRAMES.to_le_bytes());
        packet.extend_from_slice(&payload);
        self.sequence = self.sequence.wrapping_add(1);
        Ok(packet)
    }
}

/// A lagging source or writer drops sound; neither can block capture or input.
pub async fn encode(
    mut raw: broadcast::Receiver<Event>,
    mut requests: watch::Receiver<Request>,
    packets: broadcast::Sender<Packet>,
) {
    let mut current = Request::default();
    let mut encoder: Option<Encoder> = None;
    loop {
        tokio::select! {
            biased;
            changed = requests.changed() => {
                if changed.is_err() { break; }
                current = *requests.borrow_and_update();
                raw = raw.resubscribe();
                encoder = current.codec.and_then(|codec| match Encoder::new(codec) {
                    Ok(encoder) => Some(encoder),
                    Err(error) => { tracing::warn!("audio encoder unavailable: {error}"); None }
                });
            }
            event = raw.recv() => {
                match event {
                    Ok(Event::Data { bytes, received }) => {
                        let Some(encoder) = &mut encoder else { continue; };
                        if received.elapsed() > MAX_AGE { encoder.clear_pending(); continue; }
                        // A fresh fragment must not renew the age of a partial packet.
                        if encoder.pending_received.is_some_and(|first| first.elapsed() > MAX_AGE) {
                            encoder.clear_pending();
                        }
                        // Retain at most one partial packet even for a maximal RFB chunk.
                        let mut rest = bytes.as_ref();
                        while !rest.is_empty() {
                            if encoder.pending.is_empty() { encoder.pending_received = Some(received); }
                            let count = rest.len().min(PCM_BYTES - encoder.pending.len());
                            encoder.pending.extend_from_slice(&rest[..count]);
                            rest = &rest[count..];
                            if encoder.pending.len() == PCM_BYTES {
                                let received = encoder.pending_received.take().expect("partial audio ingress time");
                                let pcm = std::mem::take(&mut encoder.pending);
                                if received.elapsed() <= MAX_AGE {
                                    match encoder.packet(&pcm) {
                                        Ok(bytes) => { let _ = packets.send(Packet { generation: current.generation, received, bytes: bytes.into() }); }
                                        Err(error) => tracing::warn!("audio encoding failed: {error}"),
                                    }
                                }
                                encoder.pending = pcm;
                                encoder.pending.clear();
                            }
                        }
                    }
                    Ok(Event::Begin | Event::End) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(encoder) = &mut encoder { encoder.clear_pending(); }
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
}
