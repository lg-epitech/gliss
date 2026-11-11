//! Fuzzes the audio packetiser: raw PCM chunks of any length and age, begin
//! and end events, bursts the raw channel cannot hold and codec switches, in
//! any order.
//!
//! The packetiser task is polled by hand, so each step runs to the point
//! where it waits for input and nothing depends on a scheduler or a clock.
//! A model of the documented rules says which packets each step emits and
//! what every counter reads afterwards.

use std::{
    collections::VecDeque,
    future::Future,
    pin::pin,
    task::{Context, Waker},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::sync::broadcast::{self, error::TryRecvError};

use super::*;
use crate::{
    audio_adpcm,
    fuzz::support::{self, Rng},
};

/// What the raw channel holds for a reader that is behind, as in the RFB crate.
const RAW_CAPACITY: usize = 8;

enum Raw {
    Data { bytes: Vec<u8>, received: Instant },
    Boundary,
}

/// The packet being assembled.
struct Assembly {
    codec: Codec,
    sequence: u32,
    pcm: Vec<u8>,
    /// When its first bytes arrived.
    received: Option<Instant>,
}

/// A packet the model expects.
struct Expected {
    codec: Codec,
    generation: u64,
    sequence: u32,
    pcm: Vec<u8>,
    received: Instant,
}

struct Model {
    frames: u32,
    request: Request,
    /// Whether the request changed since the packetiser last looked.
    switched: bool,
    assembly: Option<Assembly>,
    /// Raw events sent since the packetiser last ran.
    waiting: VecDeque<Raw>,
    counters: CountersSnapshot,
}

impl Model {
    fn pcm_bytes(&self) -> usize {
        self.frames as usize * 4
    }

    /// A partial packet is dropped whole: sound never continues across a gap.
    fn reset(&mut self) {
        if let Some(assembly) = &mut self.assembly {
            if !assembly.pcm.is_empty() {
                self.counters.reset_pending_discards += 1;
                self.counters.reset_pending_bytes += assembly.pcm.len() as u64;
            }
            assembly.pcm.clear();
            assembly.received = None;
        }
    }

    /// What the control message asks for, read from `docs/audio-wire.md`:
    /// `enabled` is a boolean, and enabling names one of the three codecs.
    fn control(&mut self, value: &Value) -> bool {
        let Some(enabled) = value.get("enabled").and_then(Value::as_bool) else {
            return false;
        };
        let codec = match value.get("codec") {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) => match name.as_str() {
                "pcm" => Some(Codec::Pcm),
                "opus" => Some(Codec::Opus),
                "ima-adpcm" => Some(Codec::ImaAdpcm),
                _ => return false,
            },
            Some(_) => return false,
        };
        if enabled && codec.is_none() {
            return false;
        }
        let codec = codec.filter(|_| enabled);
        if codec != self.request.codec {
            self.request.codec = codec;
            self.request.generation = self.request.generation.wrapping_add(1);
            self.switched = true;
        }
        true
    }

    /// Everything the packetiser does when it next runs.
    fn run(&mut self) -> Vec<Expected> {
        let mut packets = Vec::new();
        if std::mem::take(&mut self.switched) {
            // A new request starts a new stream: what was assembled and what
            // waited for the old one is not sound for the new one.
            self.reset();
            self.assembly = self.request.codec.map(|codec| Assembly {
                codec,
                sequence: 0,
                pcm: Vec::new(),
                received: None,
            });
            self.waiting.clear();
            return packets;
        }
        if self.waiting.len() > RAW_CAPACITY {
            // The channel kept the newest events; the gap ends the packet.
            let lost = self.waiting.len() - RAW_CAPACITY;
            self.waiting.drain(..lost);
            self.counters.raw_lag_events += 1;
            self.counters.raw_lag_messages += lost as u64;
            self.reset();
        }
        while let Some(raw) = self.waiting.pop_front() {
            let (bytes, received) = match raw {
                Raw::Boundary => {
                    self.reset();
                    continue;
                }
                Raw::Data { bytes, received } => (bytes, received),
            };
            if self.assembly.is_none() {
                // Nobody listens: raw sound is not even counted.
                continue;
            }
            if received.elapsed() > MAX_AGE {
                self.counters.stale_raw_events += 1;
                self.counters.stale_raw_bytes += bytes.len() as u64;
                self.reset();
                continue;
            }
            let pcm_bytes = self.pcm_bytes();
            let assembly = self.assembly.as_mut().unwrap();
            for byte in bytes {
                if assembly.pcm.is_empty() {
                    assembly.received = Some(received);
                }
                assembly.pcm.push(byte);
                if assembly.pcm.len() == pcm_bytes {
                    packets.push(Expected {
                        codec: assembly.codec,
                        generation: self.request.generation,
                        sequence: assembly.sequence,
                        pcm: std::mem::take(&mut assembly.pcm),
                        received: assembly.received.take().unwrap(),
                    });
                    assembly.sequence = assembly.sequence.wrapping_add(1);
                    self.counters.emitted_packets += 1;
                    self.counters.emitted_frames += u64::from(self.frames);
                }
            }
        }
        packets
    }
}

/// Checks one packet against the wire format in `docs/audio-wire.md` and
/// returns its length.
fn assert_packet(packet: &Packet, expected: &Expected, frames: u32) -> usize {
    let bytes = &packet.bytes;
    let field = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    assert_eq!(&bytes[..4], b"GLA1");
    let codec = match expected.codec {
        Codec::Pcm => 0,
        Codec::Opus => 1,
        Codec::ImaAdpcm => 2,
    };
    assert_eq!(bytes[4..8], [codec, 2, 0, 0], "codec, channels, reserved");
    assert_eq!(field(8), 48_000);
    assert_eq!(field(12), expected.sequence, "sequence");
    assert_eq!(field(16), frames);
    assert_eq!(packet.generation, expected.generation);
    // A packet is as old as its first bytes.
    assert_eq!(packet.received, expected.received);
    let payload = &bytes[20..];
    match expected.codec {
        Codec::Pcm => assert!(payload == expected.pcm, "PCM is not the bytes that arrived"),
        Codec::ImaAdpcm => {
            assert_eq!(payload.len(), frames as usize + 7);
            let decoded = audio_adpcm::decode(payload, frames as usize).unwrap();
            // The block starts at the exact first frame.
            let first: Vec<f32> = expected.pcm.as_chunks::<2>().0[..2]
                .iter()
                .map(|sample| f32::from(i16::from_le_bytes(*sample)) / 32768.0)
                .collect();
            assert_eq!(decoded[..2], first);
        }
        Codec::Opus => {
            // The browser takes at most 4096 bytes; the encoder is asked for 1275.
            assert!((1..=1275).contains(&payload.len()));
            let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
            let mut samples = vec![0; frames as usize * 2];
            assert_eq!(
                decoder.decode(payload, &mut samples, false).unwrap(),
                frames as usize
            );
        }
    }
    bytes.len()
}

/// A control message: mostly one the browser sends, sometimes one it must not.
fn control(rng: &mut Rng) -> Value {
    let codec = rng.pick(&["pcm", "pcm", "ima-adpcm", "ima-adpcm", "opus"]);
    match rng.below(14) {
        0..7 => json!({"type": "audio", "enabled": true, "codec": codec}),
        7..10 => json!({"type": "audio", "enabled": false}),
        10 => json!({"type": "audio", "enabled": false, "codec": codec, "extra": [1, 2]}),
        11 => json!({"type": "audio", "enabled": true}),
        12 => {
            let codecs = [json!("mp3"), json!(""), json!(1), json!(null), json!("PCM")];
            let codec = &codecs[rng.below(codecs.len())];
            json!({"type": "audio", "enabled": rng.chance(2), "codec": codec})
        }
        _ => {
            let others = [
                json!({}),
                json!({"enabled": "true", "codec": "pcm"}),
                json!({"enabled": 1, "codec": "pcm"}),
                json!({"codec": "pcm"}),
                json!(null),
                json!("audio"),
            ];
            others[rng.below(others.len())].clone()
        }
    }
}

/// The length of a raw chunk. The RFB crate passes on whole frames of at most
/// 64 KiB; the packetiser is not told that and gets any length.
fn chunk(rng: &mut Rng, pcm_bytes: usize) -> usize {
    match rng.below(16) {
        0 => 0,
        1 => pcm_bytes,
        2 => pcm_bytes - 4,
        3 => pcm_bytes + 4,
        4 => 65_536,
        5 => 1 + rng.below(9),
        6 => pcm_bytes * 3,
        _ => rng.below(pcm_bytes * 2 / 4) * 4,
    }
}

/// One packetiser through `steps` steps.
fn session(rng: &mut Rng, steps: usize) {
    let duration = rng.pick(&[
        PacketDuration::Ms20,
        PacketDuration::Ms20,
        PacketDuration::Ms40,
    ]);
    let (session, requests, _status) = channel();
    let (raw, raw_receiver) = broadcast::channel(RAW_CAPACITY);
    // Room for every packet of one step, so that none is missed here.
    let (packets, mut emitted) = broadcast::channel(1024);
    let config = EncodeConfig::new(duration);
    let counters = config.counters.clone();
    // Without a budget for cooperation, the task returns only when it has
    // nothing left to read.
    let mut task = pin!(tokio::task::unconstrained(encode_with(
        raw_receiver,
        requests,
        packets,
        config
    )));
    let mut context = Context::from_waker(Waker::noop());
    let mut model = Model {
        frames: duration.frames(),
        request: Request::default(),
        switched: false,
        assembly: None,
        waiting: VecDeque::new(),
        counters: CountersSnapshot::default(),
    };
    // Sound from the future never ages, and sound of a second ago is always
    // stale, whatever the machine is busy with.
    let fresh = Instant::now() + Duration::from_secs(3600);
    let stale = Instant::now() - Duration::from_secs(1);
    for _ in 0..steps {
        // Usually one event between two runs of the task; sometimes many.
        let events = if rng.chance(8) { rng.between(2, 14) } else { 1 };
        for _ in 0..events {
            match rng.below(20) {
                0..3 => {
                    let value = control(rng);
                    let accepted = model.control(&value);
                    assert_eq!(
                        request(&session.request, value.clone()).is_ok(),
                        accepted,
                        "{value}"
                    );
                    assert_eq!(*session.request.borrow(), model.request);
                }
                3 | 4 => {
                    let event = if rng.chance(2) {
                        Event::Begin
                    } else {
                        Event::End
                    };
                    raw.send(event).unwrap();
                    model.waiting.push_back(Raw::Boundary);
                }
                _ => {
                    let length = chunk(rng, model.pcm_bytes());
                    let bytes = rng.bytes(length);
                    // The step is a fraction of a second, counted from the
                    // first chunk of a packet, which the packet must keep.
                    let received = if rng.chance(12) {
                        stale
                    } else {
                        fresh + Duration::from_micros(rng.below(1000) as u64)
                    };
                    raw.send(Event::Data {
                        bytes: bytes.clone().into(),
                        received,
                    })
                    .unwrap();
                    model.waiting.push_back(Raw::Data { bytes, received });
                }
            }
        }
        assert!(
            task.as_mut().poll(&mut context).is_pending(),
            "the packetiser ended while its channels were open"
        );
        let expected = model.run();
        for expected in &expected {
            let packet = emitted.try_recv().expect("a packet is missing");
            let length = assert_packet(&packet, expected, model.frames);
            model.counters.emitted_wire_bytes += length as u64;
        }
        assert!(
            matches!(emitted.try_recv(), Err(TryRecvError::Empty)),
            "a packet nobody asked for"
        );
        assert_eq!(counters.snapshot(), model.counters);
        // What is held is less than one packet, and never more sound than
        // arrived since the last packet.
        let held = model
            .assembly
            .as_ref()
            .map_or(0, |assembly| assembly.pcm.len());
        assert!(held < model.pcm_bytes());
    }
}

const DEFAULT_ITERATIONS: u64 = 60;

#[test]
fn fuzz_audio_packets() {
    support::run("server-audio-packets", DEFAULT_ITERATIONS, |rng, _| {
        let steps = rng.between(1, 40);
        session(rng, steps);
    });
}
