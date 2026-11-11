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
    _frames: watch::Sender<Option<Captured>>,
    notices: Arc<notices::Notices>,
    session: JoinHandle<Result<()>>,
    _audio: (
        watch::Receiver<audio::Request>,
        watch::Sender<audio::Status>,
    ),
    _input: mpsc::Receiver<Input>,
}

impl Viewer {
    fn connect(extra: &[&str]) -> Self {
        let app = app(&[&["--demo"], extra].concat());
        let (frames, frame_rx) = watch::channel(None);
        let (input, _input) = mpsc::channel(128);
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
            _frames: frames,
            notices,
            session,
            _audio: (audio_requests, audio_status),
            _input,
        }
    }

    /// The next text message, parsed. Media is not expected by the callers.
    async fn text(&mut self) -> Value {
        match self.from_server.recv().await {
            Some(Message::Text(text)) => serde_json::from_str(&text).unwrap(),
            other => panic!("text message expected, got {other:?}"),
        }
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
