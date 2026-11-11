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
    // answer to the ping shows the session is alive.
    viewer.say(json!({"type":"ping","id":7})).await;
    let (mut bells, mut clipboards, mut resizes, mut cursors) =
        (0, Vec::new(), Vec::new(), Vec::new());
    let mut answered = false;
    while !(answered
        && bells > 0
        && clipboards.last().is_some_and(|text| text == "copy 250")
        && resizes.last() == Some(&250)
        && cursors.last() == Some(&250))
    {
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

fn paused(width: u16, height: u16) -> Value {
    json!({"type":"error","message":format!("The VM display is {width}x{height}. Video needs an even width and height from 16x16 through 3840x2160, so the picture is paused until the VM changes its resolution.")})
}

impl Viewer {
    /// Lets the session do everything it can do now. The clock is paused and
    /// does not move while a frame is being encoded.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Stops reading with the writer held: one answer fits between the server
    /// and the browser, and the writer waits to hand over the next. Whatever
    /// the session produces from here on waits behind them.
    async fn hold_writer(&self, id: u64) {
        self.say(json!({"type":"ping","id":id})).await;
        self.say(json!({"type":"ping","id":id + 1})).await;
        self.settle().await;
    }

    /// Reads the two answers [`Viewer::hold_writer`] left waiting.
    async fn release_writer(&mut self, id: u64) {
        assert_eq!(self.text().await, json!({"type":"pong","id":id}));
        assert_eq!(self.text().await, json!({"type":"pong","id":id + 1}));
    }
}

#[tokio::test(start_paused = true)]
async fn a_pause_that_ends_before_its_error_is_sent_sends_no_error() {
    let mut viewer = Viewer::connect(&[]).greeted().await;
    viewer.show(64, 48);
    let first = viewer.media().await;
    viewer
        .say(json!({"type":"ack","sequence":first.sequence}))
        .await;
    viewer.hold_writer(1).await;
    // The mode cannot be encoded, and can again before the browser reads on.
    viewer.show(65, 48);
    viewer.settle().await;
    viewer.show(66, 50);
    viewer.settle().await;
    viewer.release_writer(1).await;
    // The picture is live: nothing says it is paused, before it or after.
    let resumed = viewer.media().await;
    assert_eq!(
        (resumed.width, resumed.height, resumed.keyframe),
        (66, 50, true)
    );
    viewer
        .say(json!({"type":"ack","sequence":resumed.sequence}))
        .await;
    viewer.say(json!({"type":"ping","id":3})).await;
    assert_eq!(viewer.text().await, json!({"type":"pong","id":3}));
    viewer.settle().await;
    assert!(viewer.from_server.try_recv().is_err());
    // The next pause is reported as any other.
    viewer.show(65, 48);
    assert_eq!(viewer.text().await, paused(65, 48));
    assert!(!viewer.session.is_finished());
}

#[tokio::test(start_paused = true)]
async fn the_pause_error_follows_every_frame_encoded_before_the_pause() {
    let mut viewer = Viewer::connect(&[]).greeted().await;
    viewer.show(64, 48);
    let first = viewer.media().await;
    viewer
        .say(json!({"type":"ack","sequence":first.sequence}))
        .await;
    // Many times: the writer chooses among what is ready, and an order that
    // is left to it comes out right now and then.
    for round in 0..16 {
        viewer.hold_writer(round * 2).await;
        // A frame is encoded and waits for the writer when the mode changes.
        viewer.show(64, 48);
        viewer.settle().await;
        viewer.show(65, 48);
        viewer.settle().await;
        viewer.release_writer(round * 2).await;
        let last = viewer.media().await;
        assert_eq!((last.width, last.height), (64, 48));
        assert_eq!(viewer.text().await, paused(65, 48));
        viewer
            .say(json!({"type":"ack","sequence":last.sequence}))
            .await;
        // The picture comes back for the next round.
        viewer.show(64, 48);
        let resumed = viewer.media().await;
        viewer
            .say(json!({"type":"ack","sequence":resumed.sequence}))
            .await;
        viewer.settle().await;
    }
    assert!(!viewer.session.is_finished());
}

#[tokio::test(start_paused = true)]
async fn a_browser_that_never_acknowledges_cannot_stay_by_asking_for_keyframes() {
    let mut viewer = Viewer::connect(&[]).greeted().await;
    viewer.show(64, 48);
    let first = viewer.media().await;
    assert!(first.keyframe);
    let started = tokio::time::Instant::now();
    // It reads every frame, acknowledges none, and resyncs every three seconds.
    let mut keyframes = 1;
    loop {
        tokio::select! {
            message = viewer.from_server.recv() => match message {
                Some(Message::Binary(_)) => keyframes += 1,
                Some(other) => panic!("unexpected {other:?}"),
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                let resync = json!({"type":"resync"}).to_string();
                let _ = viewer.to_server.send(Ok(Message::Text(resync.into()))).await;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "still connected after a minute and {keyframes} keyframes"
        );
    }
    let ended = viewer.session.await.unwrap().unwrap_err().to_string();
    assert!(ended.contains("did not acknowledge"), "{ended}");
    // Ten seconds from the first frame, whatever was asked for since.
    assert_eq!(started.elapsed(), scheduler::ACK_TIMEOUT);
    assert_eq!(keyframes, 4);
}

#[tokio::test(start_paused = true)]
async fn keyframes_on_request_start_at_most_four_times_a_second() {
    let mut viewer = Viewer::connect(&[]).greeted().await;
    viewer.show(64, 48);
    let first = viewer.media().await;
    viewer
        .say(json!({"type":"ack","sequence":first.sequence}))
        .await;
    // A well-behaved browser except for this: a resync every 20 ms for two seconds.
    let mut keyframes = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    for _ in 0..100 {
        tick.tick().await;
        viewer.say(json!({"type":"resync"})).await;
        while let Ok(Some(message)) =
            tokio::time::timeout(Duration::from_millis(10), viewer.from_server.recv()).await
        {
            let Message::Binary(bytes) = message else {
                panic!("unexpected {message:?}")
            };
            let packet = gliss_video::parse_packet(&bytes).unwrap();
            keyframes += u32::from(packet.keyframe);
            viewer
                .say(json!({"type":"ack","sequence":packet.sequence}))
                .await;
        }
    }
    // The first at once, then one per 250 ms.
    assert!((2..=9).contains(&keyframes), "{keyframes} keyframes");
    assert!(!viewer.session.is_finished());
}
