//! One viewer session over an in-memory socket, on a paused clock.
use super::test_support::app;
use super::*;
use futures_util::{sink, stream};
use gliss_rfb::Cursor;
use serde_json::Value;
use tokio::task::JoinHandle;

/// The browser's end of a session and the source's end of its display.
struct Viewer {
    /// One message fits between the server and a browser that is not reading.
    from_server: mpsc::Receiver<Message>,
    to_server: mpsc::Sender<Result<Message, std::io::Error>>,
    frames: watch::Sender<Option<Captured>>,
    notices: Arc<notices::Notices>,
    session: JoinHandle<Result<()>>,
    _audio: (
        watch::Receiver<audio::Request>,
        watch::Sender<audio::Status>,
    ),
    input: mpsc::Receiver<Input>,
}

/// The header of a video packet.
#[derive(Debug, PartialEq, Eq)]
struct Media {
    sequence: u32,
    width: u16,
    height: u16,
    keyframe: bool,
}

impl Viewer {
    fn connect(extra: &[&str]) -> Self {
        let app = app(&[&["--demo"], extra].concat());
        let (frames, frame_rx) = watch::channel(None);
        let (input, input_rx) = mpsc::channel(128);
        let notices = Arc::new(notices::Notices::default());
        let (audio, audio_requests, audio_status) = audio::channel();
        let mut source = Source {
            audio,
            audio_counters: Arc::new(audio::Counters::default()),
            frames: frame_rx,
            input,
            notices: notices.clone(),
            shutdown: None,
            _tasks: JoinSet::new(),
        };
        let (out, from_server) = mpsc::channel::<Message>(1);
        let (to_server, incoming) = mpsc::channel(64);
        let sink = Box::pin(sink::unfold(out, |out, message| async move {
            out.send(message)
                .await
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
            Ok::<_, std::io::Error>(out)
        }));
        let receiver = Box::pin(stream::unfold(incoming, |mut incoming| async move {
            incoming.recv().await.map(|message| (message, incoming))
        }));
        let session =
            tokio::spawn(async move { stream_session(sink, receiver, app, &mut source).await });
        Self {
            from_server,
            to_server,
            frames,
            notices,
            session,
            _audio: (audio_requests, audio_status),
            input: input_rx,
        }
    }

    /// The next text message, parsed. Media is not expected by the callers.
    async fn text(&mut self) -> Value {
        match self.from_server.recv().await {
            Some(Message::Text(text)) => serde_json::from_str(&text).unwrap(),
            other => panic!("text message expected, got {other:?}"),
        }
    }

    /// The next message, which must be a video packet.
    async fn media(&mut self) -> Media {
        match self.from_server.recv().await {
            Some(Message::Binary(bytes)) => {
                let packet = gliss_video::parse_packet(&bytes).unwrap();
                Media {
                    sequence: packet.sequence,
                    width: packet.width,
                    height: packet.height,
                    keyframe: packet.keyframe,
                }
            }
            other => panic!("video packet expected, got {other:?}"),
        }
    }

    /// The display shows a new picture of this size.
    fn show(&self, width: u16, height: u16) {
        self.frames.send_replace(Some(Captured {
            frame: Arc::new(Frame::new(width, height).unwrap()),
            forwarded_input_id: 0,
            published_at: None,
        }));
    }

    async fn say(&self, message: Value) {
        self.to_server
            .send(Ok(Message::Text(message.to_string().into())))
            .await
            .unwrap();
    }

    /// Reads the two messages every session starts with.
    async fn greeted(mut self) -> Self {
        assert_eq!(self.text().await["type"], "hello");
        assert_eq!(self.text().await["type"], "audio_status");
        self
    }
}

fn cursor(shade: u8) -> Cursor {
    Cursor {
        width: 128,
        height: 128,
        hotspot_x: 1,
        hotspot_y: 2,
        pixels: vec![shade; 128 * 128 * 4],
    }
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_guest_events_does_not_end_the_session_of_a_browser_that_is_not_reading() {
    let mut viewer = Viewer::connect(&[]).greeted().await;
    // The browser reads nothing for five seconds, half of the write timeout,
    // while the guest rings, copies, resizes and changes its cursor.
    for round in 0..=250u8 {
        viewer.notices.bell();
        viewer.notices.clipboard(format!("copy {round}"));
        viewer.notices.resize_result(640, 480, u16::from(round));
        viewer.notices.cursor(cursor(round));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !viewer.session.is_finished(),
        "{:?}",
        viewer.session.await.unwrap()
    );
    // Once it reads again it gets the newest value of each kind, and at most
    // one older value that was already being written when it stopped. The
    // cursor is written last; the answer to the ping shows the session is alive.
    viewer.say(json!({"type":"ping","id":7})).await;
    let (mut bells, mut clipboards, mut resizes, mut cursors) =
        (0, Vec::new(), Vec::new(), Vec::new());
    let mut answered = false;
    while !answered || cursors.last() != Some(&250) {
        let message = viewer.text().await;
        match message["type"].as_str().unwrap() {
            "bell" => bells += 1,
            "clipboard" => clipboards.push(message["text"].as_str().unwrap().to_owned()),
            "resize_result" => resizes.push(message["status"].as_u64().unwrap()),
            "cursor" => cursors.push(message["pixels"][0].as_u64().unwrap()),
            "pong" => answered = true,
            other => panic!("unexpected {other}"),
        }
    }
    assert!((1..=2).contains(&bells), "{bells} bells");
    assert!(
        clipboards.len() <= 2 && clipboards.last().is_some_and(|text| text == "copy 250"),
        "{clipboards:?}"
    );
    assert!(
        resizes.len() <= 2 && resizes.last() == Some(&250),
        "{resizes:?}"
    );
    assert!(
        cursors.len() <= 2 && cursors.last() == Some(&250),
        "{cursors:?}"
    );
    assert!(!viewer.session.is_finished());
}

#[tokio::test(start_paused = true)]
async fn a_display_mode_h264_cannot_encode_pauses_the_picture_and_keeps_the_session() {
    let mut viewer = Viewer::connect(&[]).greeted().await;
    viewer.show(64, 48);
    let first = viewer.media().await;
    assert_eq!((first.width, first.height, first.keyframe), (64, 48, true));
    viewer
        .say(json!({"type":"ack","sequence":first.sequence}))
        .await;
    let paused = |width: u16, height: u16| json!({"type":"error","message":format!("The VM display is {width}x{height}. Video needs an even width and height from 16x16 through 3840x2160, so the picture is paused until the VM changes its resolution.")});
    // Odd, too small and too large: each mode is reported when it appears.
    for (width, height) in [
        (65, 48),
        (64, 47),
        (14, 48),
        (64, 2),
        (3842, 16),
        (2162, 2162),
    ] {
        viewer.show(width, height);
        assert_eq!(viewer.text().await, paused(width, height));
        // A second picture in the same mode says nothing new.
        viewer.show(width, height);
    }
    // Input still reaches the VM, so the user can change the mode from here.
    viewer
        .say(json!({"type":"key","keysym":65,"down":true,"id":1}))
        .await;
    viewer
        .say(json!({"type":"pointer","x":3,"y":4,"buttons":1,"id":2}))
        .await;
    viewer
        .say(json!({"type":"resize","width":66,"height":50,"id":3}))
        .await;
    assert!(matches!(
        viewer.input.recv().await,
        Some(Input::Key {
            keysym: 65,
            down: true,
            ..
        })
    ));
    assert!(matches!(
        viewer.input.recv().await,
        Some(Input::Pointer {
            x: 3,
            y: 4,
            buttons: 1,
            ..
        })
    ));
    assert!(matches!(
        viewer.input.recv().await,
        Some(Input::Resize {
            width: 66,
            height: 50,
            ..
        })
    ));
    // A minute in that mode: no picture, no repeated report, and the session is still there.
    tokio::time::sleep(Duration::from_secs(60)).await;
    viewer.say(json!({"type":"ping","id":9})).await;
    assert_eq!(viewer.text().await, json!({"type":"pong","id":9}));
    // The picture comes back by itself with the first mode that fits.
    viewer.show(66, 50);
    let resumed = viewer.media().await;
    assert_eq!(
        (resumed.width, resumed.height, resumed.keyframe),
        (66, 50, true)
    );
    viewer
        .say(json!({"type":"ack","sequence":resumed.sequence}))
        .await;
    // Leaving and re-entering a mode is a mode change: it is reported again.
    viewer.show(65, 48);
    assert_eq!(viewer.text().await, paused(65, 48));
    viewer.show(66, 50);
    let again = viewer.media().await;
    assert_eq!((again.width, again.height), (66, 50));
    assert!(!viewer.session.is_finished());
}
