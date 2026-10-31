use super::*;
use tokio::sync::{Mutex as AsyncMutex, oneshot};

#[derive(Clone, Copy, Debug)]
enum Step {
    Loss,
    Early429,
    Interrupt,
    Terminal,
    Disconnect,
    Continuation,
}

enum Command {
    Terminal(oneshot::Sender<()>),
    Loss(oneshot::Sender<()>),
    Quota(oneshot::Sender<()>),
}
struct ServerTask(tokio::task::JoinHandle<()>);
impl Drop for ServerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn permutations(steps: &mut [Step; 6], at: usize, out: &mut Vec<[Step; 6]>) {
    if at == steps.len() {
        out.push(*steps);
        return;
    }
    for index in at..steps.len() {
        steps.swap(at, index);
        permutations(steps, at + 1, out);
        steps.swap(at, index);
    }
}

async fn barrier(ws: &mut Client, events: &mut Vec<Value>) {
    send(ws, r#"{"type":"response.interrupt"}"#).await;
    loop {
        let event = recv(ws).await;
        if event["error"]["message"] == "response.interrupt requires response_id" {
            break;
        }
        events.push(event);
    }
}

async fn begin(peer: &mut Peer, id: &str, event_id: &str) {
    peer.write(&json!({"type":"response.created","response":{"id":id}}).to_string())
        .await;
    peer.write(&json!({"type":"response.output_text.delta","delta":event_id}).to_string())
        .await;
}

async fn complete(peer: &mut Peer, id: &str) {
    peer.write(&json!({"type":"response.completed","response":{"id":id,"output":[]}}).to_string())
        .await;
}

async fn controlled(
    immediate: bool,
    creates: Arc<std::sync::Mutex<Vec<String>>>,
    interrupted: Arc<Notify>,
    ready: Arc<Notify>,
) -> (Upstream, mpsc::Sender<Command>) {
    let (commands, rx) = mpsc::channel(2);
    let rx = Arc::new(AsyncMutex::new(rx));
    let server = Upstream::start(move |mut peer| {
        let rx = rx.clone(); let creates = creates.clone();
        let interrupted = interrupted.clone(); let ready = ready.clone();
        async move {
            let initial = peer.read().await.unwrap();
            creates.lock().unwrap().push(at(&initial, "/event_id"));
            ready.notify_one();
            let mut created = immediate;
            if immediate { begin(&mut peer, "r1", "c1").await; }
            let mut terminal = false;
            let mut queued = false;
            let mut rx = rx.lock().await;
            loop {
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(Command::Loss(done)) => { let _ = done.send(()); return; }
                        Some(Command::Quota(done)) => {
                            assert!(!created, "early 429 must precede accepted output");
                            peer.write(r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"early rejection","resets_in_seconds":120}}"#).await;
                            let _ = done.send(());
                        }
                        Some(Command::Terminal(done)) => {
                            terminal = true;
                            if !created { begin(&mut peer, "r1", "c1").await; created = true; }
                            complete(&mut peer, "r1").await;
                            if queued { begin(&mut peer, "r2", "c2").await; complete(&mut peer, "r2").await; queued = false; }
                            let _ = done.send(());
                        }
                        None => return,
                    },
                    frame = peer.read() => {
                        let Some(frame) = frame else { return; };
                        match at(&frame, "/type").as_str() {
                            "response.interrupt" => {
                                assert!(!terminal, "{ROUND1_INVARIANT}: interrupt after terminal");
                                interrupted.notify_one();
                            }
                            "response.create" => {
                                creates.lock().unwrap().push(at(&frame, "/event_id"));
                                if terminal { begin(&mut peer, "r2", "c2").await; complete(&mut peer, "r2").await; } else { queued = true; }
                            }
                            other => panic!("unexpected upstream frame {other}"),
                        }
                    }
                }
            }
        }
    }).await;
    (server, commands)
}

async fn schedule(steps: [Step; 6], duplex: bool) {
    let creates = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let interrupted = Arc::new(Notify::new());
    let ready = Arc::new(Notify::new());
    let (mut first, first_commands) =
        controlled(false, creates.clone(), interrupted.clone(), ready.clone()).await;
    let (mut sibling, sibling_commands) = controlled(
        true,
        creates.clone(),
        interrupted.clone(),
        Arc::new(Notify::new()),
    )
    .await;
    let (url, task) = retry_proxy_server(&steering(duplex), &first.url, &sibling.url).await;
    let _server = ServerTask(task);
    let mut ws = connect(&url, &[]).await;
    send(
        &mut ws,
        r#"{"type":"response.create","model":"round1-invariant-model","input":[],"event_id":"c1"}"#,
    )
    .await;
    tokio::time::timeout(CLIENT_WAIT, ready.notified())
        .await
        .expect(ROUND1_INVARIANT);
    let (mut retried, mut lost, mut terminal, mut closed) = (false, false, false, false);
    let mut events = Vec::new();
    for step in steps {
        if closed {
            continue;
        }
        let commands = if retried {
            &sibling_commands
        } else {
            &first_commands
        };
        match step {
            Step::Early429 if !lost && !terminal => {
                let (done, complete) = oneshot::channel();
                first_commands.send(Command::Quota(done)).await.unwrap();
                complete.await.unwrap();
                loop {
                    let event = recv(&mut ws).await;
                    let ready = event["delta"] == "c1";
                    events.push(event);
                    if ready { break; }
                }
                retried = true;
                first.finished().await;
            }
            Step::Loss if !lost => {
                let (done, complete) = oneshot::channel();
                commands.send(Command::Loss(done)).await.unwrap();
                complete.await.unwrap();
                if retried { sibling.finished().await; } else { first.finished().await; }
                lost = true;
            }
            Step::Terminal if !lost => {
                let (done, complete) = oneshot::channel();
                commands.send(Command::Terminal(done)).await.unwrap();
                complete.await.unwrap();
                loop {
                    let event = recv(&mut ws).await;
                    let ready = event["type"] == "response.completed" && event["response"]["id"] == "r1";
                    events.push(event);
                    if ready { break; }
                }
                terminal = true;
            }
            Step::Interrupt => {
                send(&mut ws, r#"{"type":"response.interrupt","response_id":"r1"}"#).await;
                if !lost && !terminal {
                    tokio::time::timeout(CLIENT_WAIT, interrupted.notified()).await.expect(ROUND1_INVARIANT);
                }
            }
            Step::Continuation => send(&mut ws, r#"{"type":"response.create","model":"round1-invariant-model","input":[],"previous_response_id":"r1","event_id":"c2"}"#).await,
            Step::Disconnect => { barrier(&mut ws, &mut events).await; ws.close(None).await.unwrap(); closed = true; }
            _ => {}
        }
        if !closed {
            barrier(&mut ws, &mut events).await;
        }
    }
    if !lost {
        if retried {
            sibling.finished().await;
        } else {
            first.finished().await;
        }
    }
    let context = format!("{steps:?}, duplex={duplex}: {ROUND1_INVARIANT}");
    assert_eq!(first.connections(), 1, "{context}");
    assert_eq!(sibling.connections(), usize::from(retried), "{context}");
    let creates = creates.lock().unwrap();
    assert_eq!(
        creates.iter().filter(|id| id.as_str() == "c1").count(),
        1 + usize::from(retried),
        "only early rejection permits another attempt: {context}"
    );
    assert!(
        creates.iter().filter(|id| id.as_str() == "c2").count() <= 1,
        "continuation replayed: {context}"
    );
    for id in ["c1", "c2"] {
        assert!(
            events.iter().filter(|event| event["delta"] == id).count() <= 1,
            "replayed output {id}: {context}"
        );
        assert!(
            events
                .iter()
                .filter(|event| event["error"]["code"] == "upstream_websocket_lost"
                    && event["error"]["event_id"] == id)
                .count()
                <= 1,
            "duplicate loss {id}: {context}"
        );
    }
}

#[tokio::test]
async fn invariant_family_all_six_step_interleavings() {
    let mut shapes = Vec::new();
    permutations(
        &mut [
            Step::Loss,
            Step::Early429,
            Step::Interrupt,
            Step::Terminal,
            Step::Disconnect,
            Step::Continuation,
        ],
        0,
        &mut shapes,
    );
    assert_eq!(shapes.len(), 720);
    for duplex in [false, true] {
        for shape in &shapes {
            schedule(*shape, duplex).await;
        }
    }
}
