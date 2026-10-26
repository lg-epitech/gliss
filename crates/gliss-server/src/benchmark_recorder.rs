//! Opt-in, operator-owned evidence from the existing canonical capture path.
//! No additional display connection and no reference pixels on media/control.
use super::*;
use flate2::{Compression, write::ZlibEncoder};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Weak, mpsc as channel},
    thread,
};

const CAP: usize = 128 * 1024 * 1024;
const MAX_RECORDS: u64 = 4096;
const CAMERA: [usize; 4] = [120, 126, 192, 108];
const TEXT: [usize; 4] = [24, 422, 1232, 96];
const RAW_BYTES: usize = (192 * 108 + 1232 * 96) * 4;

struct CaptureRecord {
    generation: u64,
    at_ns: u64,
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

#[derive(Default)]
struct State {
    failure: Option<String>,
    captures: u64,
    associations: u64,
    written_bytes: usize,
    finished: bool,
}

type FrameIdentity = (Weak<Frame>, u64);

pub(super) struct Recorder {
    directory: PathBuf,
    captures: Mutex<Option<channel::SyncSender<CaptureRecord>>>,
    associations: Mutex<Option<channel::SyncSender<serde_json::Value>>>,
    identities: Mutex<HashMap<usize, FrameIdentity>>,
    generation: AtomicU64,
    state: Arc<Mutex<State>>,
    started: Instant,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

fn new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    Ok(options.open(path)?)
}

fn directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && !path.exists(),
        "benchmark directory must be fresh and absolute"
    );
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
        "benchmark directory has unsafe components"
    );
    for ancestor in path.ancestors().skip(1) {
        let info = fs::symlink_metadata(ancestor).context("benchmark parent directory missing")?;
        ensure!(
            info.is_dir() && !info.file_type().is_symlink(),
            "benchmark parent is not a real directory"
        );
        #[cfg(unix)]
        {
            // SAFETY: geteuid has no memory arguments and changes no process state.
            let uid = unsafe { libc::geteuid() };
            ensure!(
                (info.uid() == uid || info.uid() == 0) && info.mode() & 0o022 == 0,
                "benchmark parent ownership or writable permissions differ"
            );
        }
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)?;
    Ok(())
}

fn extract(frame: &Frame) -> Result<Vec<u8>> {
    ensure!(frame.is_valid(), "benchmark capture storage invalid");
    let mut raw = Vec::with_capacity(RAW_BYTES);
    for [x, y, width, height] in [CAMERA, TEXT] {
        ensure!(
            x + width <= usize::from(frame.width) && y + height <= usize::from(frame.height),
            "benchmark fixed ROI outside capture geometry"
        );
        for row in y..y + height {
            let offset = (row * usize::from(frame.width) + x) * 4;
            raw.extend_from_slice(&frame.pixels[offset..offset + width * 4]);
        }
    }
    ensure!(raw.len() == RAW_BYTES, "benchmark ROI byte shape");
    Ok(raw)
}

fn json_line(value: &serde_json::Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= 4096, "benchmark metadata record bound");
    Ok(bytes)
}

impl Recorder {
    pub(super) fn create(path: PathBuf) -> Result<Arc<Self>> {
        directory(&path)?;
        let pixels = new_file(&path.join("roi.rgba.zlib"))?;
        let metadata = new_file(&path.join("captures.jsonl"))?;
        let associations_file = new_file(&path.join("associations.jsonl"))?;
        let (capture_tx, capture_rx) = channel::sync_channel(4);
        let (association_tx, association_rx) = channel::sync_channel(4096);
        let state = Arc::new(Mutex::new(State::default()));
        let thread_state = state.clone();
        let output = path.clone();
        let worker = thread::Builder::new().name("gliss-roi-recorder".into()).spawn(move || {
            let result = write_records(&output, pixels, metadata, associations_file, capture_rx, association_rx, &thread_state);
            let mut status = thread_state.lock().unwrap();
            let mut failed_receipt=None;
            if let Err(error) = result {
                status.failure.get_or_insert_with(|| format!("{error:#}"));
                failed_receipt=Some(json!({"schema":"gliss-canonical-roi-recorder-v1","status":"failed",
                    "failure":status.failure,"captures":status.captures,"associations":status.associations,
                    "written_bytes":status.written_bytes,"byte_cap":CAP,
                    "partial_files_retained":true}));
            }
            status.finished = true;
            drop(status);
            if let Some(failed)=failed_receipt
                && let Ok(mut file)=new_file(&output.join("manifest.json"))
                && let Ok(bytes)=serde_json::to_vec_pretty(&failed) {
                let _=file.write_all(&bytes);
            }
        })?;
        Ok(Arc::new(Self {
            directory: path,
            captures: Mutex::new(Some(capture_tx)),
            associations: Mutex::new(Some(association_tx)),
            identities: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            state,
            started: Instant::now(),
            worker: Mutex::new(Some(worker)),
        }))
    }

    fn fail(&self, message: &str) -> anyhow::Error {
        self.state
            .lock()
            .unwrap()
            .failure
            .get_or_insert_with(|| message.to_owned());
        anyhow::anyhow!("{message}")
    }

    pub(super) fn check(&self) -> Result<()> {
        if let Some(error) = &self.state.lock().unwrap().failure {
            bail!("benchmark evidence invalid: {error}");
        }
        Ok(())
    }

    pub(super) fn capture(&self, frame: &Arc<Frame>) -> Result<()> {
        self.check()?;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        if generation > MAX_RECORDS {
            return Err(self.fail("benchmark capture record cap exceeded"));
        }
        let record = CaptureRecord {
            generation,
            at_ns: self.now_ns(),
            width: frame.width,
            height: frame.height,
            pixels: extract(frame)?,
        };
        let sender = self.captures.lock().unwrap();
        let sent = sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(record).is_ok());
        if !sent {
            return Err(self.fail("benchmark raw ROI queue full or closed"));
        }
        let mut identities = self.identities.lock().unwrap();
        identities.retain(|_, (weak, _)| weak.strong_count() > 0);
        identities.insert(
            Arc::as_ptr(frame) as usize,
            (Arc::downgrade(frame), generation),
        );
        Ok(())
    }

    pub(super) fn generation(&self, frame: &Arc<Frame>) -> Result<u64> {
        self.check()?;
        self.identities
            .lock()
            .unwrap()
            .get(&(Arc::as_ptr(frame) as usize))
            .filter(|(weak, _)| {
                weak.upgrade()
                    .is_some_and(|owned| Arc::ptr_eq(&owned, frame))
            })
            .map(|(_, generation)| *generation)
            .context("encoded frame lacks canonical capture association")
    }

    pub(super) fn associate(
        &self,
        generation: u64,
        epoch: u64,
        sequence: u32,
        protocol: &str,
    ) -> Result<()> {
        self.check()?;
        ensure!(
            generation > 0 && generation <= self.generation.load(Ordering::Relaxed),
            "benchmark capture generation unknown"
        );
        let value = json!({"capture_generation":generation,"epoch":epoch,"sequence":sequence,
            "protocol":protocol,"at_ns":self.now_ns()});
        let sender = self.associations.lock().unwrap();
        if !sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(value).is_ok())
        {
            return Err(self.fail("benchmark association queue full or closed"));
        }
        Ok(())
    }

    fn now_ns(&self) -> u64 {
        self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
    }

    pub(super) fn snapshot(&self) -> serde_json::Value {
        let state = self.state.lock().unwrap();
        json!({"directory":self.directory,"failure":state.failure,"captures_written":state.captures,
            "associations_written":state.associations,"written_bytes":state.written_bytes,"finished":state.finished,
            "capture_queue_records":4,"raw_record_bytes":RAW_BYTES,"byte_cap":CAP})
    }

    pub(super) fn finish(&self) -> Result<()> {
        self.captures.lock().unwrap().take();
        self.associations.lock().unwrap().take();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("benchmark recorder thread panicked"))?;
        }
        self.check()
    }
}

fn write_records(
    directory: &Path,
    mut pixels: File,
    mut metadata: File,
    mut associations: File,
    captures: channel::Receiver<CaptureRecord>,
    association_rx: channel::Receiver<serde_json::Value>,
    state: &Arc<Mutex<State>>,
) -> Result<()> {
    let mut offset = 0usize;
    let mut bytes = 0usize;
    let mut pixel_sha = Sha256::new();
    let mut capture_sha = Sha256::new();
    let mut association_sha = Sha256::new();
    let mut capture_count = 0u64;
    let mut association_count = 0u64;
    let mut capture_closed = false;
    let mut associations_closed = false;
    while !capture_closed || !associations_closed {
        if !capture_closed {
            match captures.recv_timeout(Duration::from_millis(10)) {
                Ok(record) => {
                    let mut compressor = ZlibEncoder::new(Vec::new(), Compression::fast());
                    compressor.write_all(&record.pixels)?;
                    let compressed = compressor.finish()?;
                    let row = json_line(
                        &json!({"generation":record.generation,"at_ns":record.at_ns,
                        "width":record.width,"height":record.height,"offset":offset,"compressed_bytes":compressed.len(),
                        "raw_bytes":record.pixels.len(),"raw_sha256":hex::encode(Sha256::digest(&record.pixels)),
                        "compressed_sha256":hex::encode(Sha256::digest(&compressed)),"camera":CAMERA,"text":TEXT}),
                    )?;
                    bytes = bytes
                        .checked_add(compressed.len() + row.len())
                        .context("benchmark byte accounting overflow")?;
                    ensure!(bytes <= CAP, "benchmark native artifact byte cap exceeded");
                    pixels.write_all(&compressed)?;
                    metadata.write_all(&row)?;
                    pixel_sha.update(&compressed);
                    capture_sha.update(&row);
                    offset += compressed.len();
                    capture_count += 1;
                }
                Err(channel::RecvTimeoutError::Timeout) => {}
                Err(channel::RecvTimeoutError::Disconnected) => capture_closed = true,
            }
        }
        for _ in 0..64 {
            match association_rx.try_recv() {
                Ok(value) => {
                    association_count += 1;
                    ensure!(
                        association_count <= 16384,
                        "benchmark association record cap exceeded"
                    );
                    let row = json_line(&value)?;
                    bytes = bytes
                        .checked_add(row.len())
                        .context("benchmark byte accounting overflow")?;
                    ensure!(bytes <= CAP, "benchmark native artifact byte cap exceeded");
                    associations.write_all(&row)?;
                    association_sha.update(&row);
                }
                Err(channel::TryRecvError::Empty) => break,
                Err(channel::TryRecvError::Disconnected) => {
                    associations_closed = true;
                    break;
                }
            }
        }
        let mut current = state.lock().unwrap();
        current.captures = capture_count;
        current.associations = association_count;
        current.written_bytes = bytes;
    }
    pixels.sync_all()?;
    metadata.sync_all()?;
    associations.sync_all()?;
    let failure = state.lock().unwrap().failure.clone();
    let manifest = json!({"schema":"gliss-canonical-roi-recorder-v1","status":if failure.is_none(){"complete"}else{"failed"},
        "failure":failure,"camera":CAMERA,"text":TEXT,"raw_record_bytes":RAW_BYTES,"byte_cap":CAP,
        "capture_queue_records":4,"captures":capture_count,"associations":association_count,"written_bytes":bytes,
        "pixels":{"path":"roi.rgba.zlib","bytes":offset,"sha256":hex::encode(pixel_sha.finalize())},
        "captures_sha256":hex::encode(capture_sha.finalize()),"associations_sha256":hex::encode(association_sha.finalize()),
        "scope":"Existing canonical capture pixels; local monotonic clock; no second display peer or wire reference channel."});
    let raw = serde_json::to_vec_pretty(&manifest)?;
    ensure!(
        bytes + raw.len() <= CAP,
        "benchmark final manifest exceeds artifact cap"
    );
    new_file(&directory.join("manifest.json"))?.write_all(&raw)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_camera_and_text_rois_have_fixed_bytes_and_do_not_shift() {
        let mut frame = Frame::new(1280, 800).unwrap();
        let offset = (126 * 1280 + 120) * 4;
        frame.pixels[offset] = 77;
        let offset = (422 * 1280 + 24) * 4;
        frame.pixels[offset] = 99;
        let raw = extract(&frame).unwrap();
        assert_eq!(raw.len(), 556032);
        assert_eq!(raw[0], 77);
        assert_eq!(raw[192 * 108 * 4], 99);
        assert!(extract(&Frame::new(640, 480).unwrap()).is_err());
    }
}
