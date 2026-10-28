//! Audio ingress reference, without encoding or video streaming. Assembly ages are modeled.
use anyhow::{Context, Result, ensure};
use gliss_rfb::audio::Event;
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

const AGE: Duration = Duration::from_millis(100);
const WARMUP: Duration = Duration::from_millis(1500);
const WINDOW: Duration = Duration::from_secs(6);

struct Model {
    target: usize,
    pending: usize,
    oldest: Option<Instant>,
    ages_ms: Vec<f64>,
    discarded_bytes: usize,
}

impl Model {
    fn new(frames: usize) -> Self {
        Self {
            target: frames * 4,
            pending: 0,
            oldest: None,
            ages_ms: Vec::new(),
            discarded_bytes: 0,
        }
    }

    fn clear(&mut self) {
        self.discarded_bytes += self.pending;
        self.pending = 0;
        self.oldest = None;
    }

    fn feed(&mut self, mut bytes: usize, received: Instant, now: Instant) -> Result<()> {
        if now.duration_since(received) > AGE {
            self.clear();
            self.discarded_bytes += bytes;
            return Ok(());
        }
        if self
            .oldest
            .is_some_and(|oldest| now.duration_since(oldest) > AGE)
        {
            self.clear();
        }
        while bytes > 0 {
            if self.pending == 0 {
                self.oldest = Some(received);
            }
            let count = bytes.min(self.target - self.pending);
            self.pending += count;
            bytes -= count;
            if self.pending == self.target {
                ensure!(self.ages_ms.len() < 1024, "modeled packet cap");
                self.ages_ms.push(
                    now.duration_since(self.oldest.take().context("missing ingress")?)
                        .as_secs_f64()
                        * 1000.0,
                );
                self.pending = 0;
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> Value {
        json!({"packet_frames":self.target/4,"completed":self.ages_ms.len(),
            "assembly_age_ms":self.ages_ms,"discarded_bytes":self.discarded_bytes,"pending_bytes":self.pending})
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 3,
        "supply owned VNC endpoint and fresh output"
    );
    let port: u16 = args[1]
        .strip_prefix("127.0.0.1:")
        .context("loopback endpoint required")?
        .parse()?;
    ensure!(
        port > 0 && args[1] == format!("127.0.0.1:{port}"),
        "canonical loopback endpoint required"
    );
    let path = Path::new(&args[2]);
    ensure!(!path.exists(), "output must be fresh");
    let (mut reader, mut writer, info) = gliss_rfb::connect(&args[1], None).await?;
    let mut available = writer.audio_available();
    let mut events = reader.audio_events();
    writer
        .request_update(false, info.width, info.height)
        .await?;
    let pump = tokio::spawn(async move { while reader.read_event().await.is_ok() {} });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !*available.borrow() {
            available.changed().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("audio announcement timeout")??;
    writer.set_audio(true).await?;
    let warmup_end = Instant::now() + WARMUP;
    while Instant::now() < warmup_end {
        let remaining = warmup_end.saturating_duration_since(Instant::now());
        if tokio::time::timeout(remaining, events.recv())
            .await
            .is_err()
        {
            break;
        }
    }
    events = events.resubscribe();
    let start = Instant::now();
    let mut rows = Vec::new();
    let mut lagged = 0_u64;
    let mut transitions = 0;
    let mut short = Model::new(960);
    let mut long = Model::new(1920);
    let mut bytes = 0;
    while start.elapsed() < WINDOW {
        let remaining = WINDOW.saturating_sub(start.elapsed());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(Event::Data {
                bytes: data,
                received,
            })) => {
                let now = Instant::now();
                if now.duration_since(start) >= WINDOW {
                    break;
                }
                ensure!(
                    rows.len() < 2048 && data.len() <= 65_536 && data.len().is_multiple_of(4),
                    "raw record cap/format"
                );
                bytes += data.len();
                ensure!(bytes <= 2_097_152, "raw byte budget");
                let relative = if received >= start {
                    received.duration_since(start).as_secs_f64()
                } else {
                    -start.duration_since(received).as_secs_f64()
                };
                rows.push(json!({"bytes":data.len(),"ingress_ms":relative*1000.0,
                    "consumed_ms":now.duration_since(start).as_secs_f64()*1000.0,"ingress_to_consumer_ms":now.duration_since(received).as_secs_f64()*1000.0}));
                short.feed(data.len(), received, now)?;
                long.feed(data.len(), received, now)?;
            }
            Ok(Ok(Event::Begin | Event::End)) => {
                transitions += 1;
                short.clear();
                long.clear();
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                lagged += n;
                short.clear();
                long.clear();
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                anyhow::bail!("audio source closed")
            }
            Err(_) => break,
        }
    }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let disable = tokio::time::timeout(Duration::from_secs(1), writer.set_audio(false)).await;
    pump.abort();
    let _ = pump.await;
    ensure!(
        disable.context("disable timeout")?.is_ok(),
        "audio disable failed"
    );
    let report = json!({"schema":"gliss-audio-ingress-reference-v1","window_ms":6000,"elapsed_ms":elapsed,
        "raw_bytes":bytes,"events":rows,"raw_lagged_messages":lagged,"transitions":transitions,
        "models":[short.snapshot(),long.snapshot()],"audio_disabled":true,
        "qualification":"Actual RFB ingress events; models exclude encoding, writer queueing, video streaming and guest-source age. No CPU/latency improvement claim."});
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, &report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_live_twenty_ms_chunks_add_twenty_ms_to_long_packet_ingress_age() {
        let start = Instant::now();
        let mut short = Model::new(960);
        let mut long = Model::new(1920);
        for now in [start, start + Duration::from_millis(20)] {
            short.feed(3840, now, now).unwrap();
            long.feed(3840, now, now).unwrap();
        }
        assert_eq!(short.ages_ms, [0.0, 0.0]);
        assert_eq!(long.ages_ms, [20.0]);
    }

    #[test]
    fn stale_partial_is_discarded_and_a_whole_batch_can_complete_at_zero_ingress_age() {
        let start = Instant::now();
        let mut model = Model::new(1920);
        model.feed(3840, start, start).unwrap();
        let now = start + Duration::from_millis(101);
        model.feed(3840, now, now).unwrap();
        assert_eq!(model.discarded_bytes, 3840);
        assert!(model.ages_ms.is_empty());
        model.feed(3840, now, now).unwrap();
        assert_eq!(model.ages_ms, [0.0]);
        model.clear();
        model.feed(7680, now, now).unwrap();
        assert_eq!(model.ages_ms, [0.0, 0.0]);
    }
}
