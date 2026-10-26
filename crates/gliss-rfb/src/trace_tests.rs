use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
#[derive(Default)]
struct Observed {
    rows: Mutex<Vec<trace::Record>>,
    actors: AtomicUsize,
}
impl trace::Observer for Observed {
    fn record(&self, value: trace::Record) {
        self.rows.lock().unwrap().push(value);
    }
    fn actor_started(&self) -> bool {
        self.actors.fetch_add(1, Ordering::SeqCst);
        true
    }
    fn actor_stopped(&self) {
        self.actors.fetch_sub(1, Ordering::SeqCst);
    }
}
fn info() -> ServerInfo {
    ServerInfo {
        width: 16,
        height: 16,
        name: "trace-test".into(),
        protocol_version: (3, 8),
    }
}
async fn absent(observed: &Observed) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while observed.actors.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn actual_queue_write_empty_update_and_dequeue_have_separate_ordinals() {
    let observed = Arc::new(Observed::default());
    let sink: trace::Sink = Some(observed.clone());
    let (client, mut peer) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(client);
    let (mut reader, mut writer, _) =
        start_tasks_traced(read, write, Frame::new(16, 16).unwrap(), info(), sink).unwrap();
    assert_eq!(observed.actors.load(Ordering::SeqCst), 2); // before either task's first poll
    writer.request_update(false, 16, 16).await.unwrap();
    writer
        .send_input(&Input::Key {
            keysym: 97,
            down: true,
            id: 41,
        })
        .await
        .unwrap();
    let mut request = [0; 10];
    peer.read_exact(&mut request).await.unwrap();
    assert_eq!(request[0], 3);
    let mut key = [0; 8];
    peer.read_exact(&mut key).await.unwrap();
    assert_eq!(key[0], 4);
    peer.write_all(&[0, 0, 0, 0]).await.unwrap();
    assert!(matches!(
        reader.read_event().await.unwrap(),
        Event::Frame(_)
    ));
    assert_eq!(reader.traced_update_ordinal(), Some(1));
    let rows = observed.rows.lock().unwrap().clone();
    assert!(rows.iter().any(|r| r.kind == trace::Kind::CommandQueued
        && r.ordinal == 1
        && r.capture_request
        && r.input_id.is_none()
        && r.outcome == trace::Outcome::Accepted));
    assert!(rows.iter().any(|r| r.kind == trace::Kind::WriteCompleted
        && r.ordinal == 2
        && !r.capture_request
        && r.input_id == Some(41)
        && r.outcome == trace::Outcome::Accepted));
    assert!(rows.iter().any(|r| r.kind == trace::Kind::UpdateDecoded
        && r.ordinal == 1
        && r.outcome == trace::Outcome::Empty));
    assert!(
        rows.iter()
            .any(|r| r.kind == trace::Kind::EventDequeued && r.ordinal == 1)
    );
    drop(reader);
    drop(writer);
    absent(&observed).await;
}
#[tokio::test]
async fn interrupted_actual_partial_write_is_canceled_never_successful() {
    let observed = Arc::new(Observed::default());
    let sink: trace::Sink = Some(observed.clone());
    let (client, _peer) = tokio::io::duplex(1);
    let (read, write) = tokio::io::split(client);
    let (reader, mut writer, _) =
        start_tasks_traced(read, write, Frame::new(16, 16).unwrap(), info(), sink).unwrap();
    let task = tokio::spawn(async move {
        writer
            .send_input(&Input::Key {
                keysym: 97,
                down: true,
                id: 99,
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if observed
                .rows
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.kind == trace::Kind::WriteStarted)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    drop(reader);
    absent(&observed).await;
    let rows = observed.rows.lock().unwrap();
    assert!(rows.iter().any(|r| r.kind == trace::Kind::WriteCompleted
        && r.input_id == Some(99)
        && r.outcome == trace::Outcome::Canceled));
    assert!(!rows.iter().any(|r| r.kind == trace::Kind::WriteCompleted
        && r.input_id == Some(99)
        && r.outcome == trace::Outcome::Accepted));
}

#[tokio::test]
async fn malformed_actual_update_is_error_instead_of_cancellation_or_frame() {
    let observed = Arc::new(Observed::default());
    let sink: trace::Sink = Some(observed.clone());
    let (client, mut peer) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(client);
    let (mut reader, writer, _) =
        start_tasks_traced(read, write, Frame::new(16, 16).unwrap(), info(), sink).unwrap();
    peer.write_all(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1, 0x7f, 0xff, 0xff, 0xff])
        .await
        .unwrap();
    assert!(reader.read_event().await.is_err());
    drop(reader);
    drop(writer);
    absent(&observed).await;
    let rows = observed.rows.lock().unwrap();
    assert!(rows.iter().any(|r| r.kind == trace::Kind::UpdateDecoded
        && r.ordinal == 1
        && r.outcome == trace::Outcome::Error));
    assert!(
        !rows
            .iter()
            .any(|r| r.kind == trace::Kind::UpdateDecoded && r.outcome == trace::Outcome::Canceled)
    );
    assert!(!rows.iter().any(|r| r.kind == trace::Kind::CloneStarted));
}

#[tokio::test]
async fn second_actor_refusal_unwinds_first_guard_before_any_task_is_spawned() {
    #[derive(Default)]
    struct RefuseSecond {
        attempts: AtomicUsize,
        active: AtomicUsize,
    }
    impl trace::Observer for RefuseSecond {
        fn record(&self, _: trace::Record) {
            panic!("no actor should run");
        }
        fn actor_started(&self) -> bool {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                self.active.fetch_add(1, Ordering::SeqCst);
                true
            } else {
                false
            }
        }
        fn actor_stopped(&self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let observed = Arc::new(RefuseSecond::default());
    let sink: trace::Sink = Some(observed.clone());
    let (client, _peer) = tokio::io::duplex(1);
    let (read, write) = tokio::io::split(client);
    assert!(start_tasks_traced(read, write, Frame::new(16, 16).unwrap(), info(), sink).is_err());
    assert_eq!(observed.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(observed.active.load(Ordering::SeqCst), 0);
}
