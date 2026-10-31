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
    Steer,
}

enum Command {
    Begin(oneshot::Sender<()>),
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

fn permutations(steps: &mut [Step], at: usize, out: &mut Vec<Vec<Step>>) {
    if at == steps.len() {
        out.push(steps.to_vec());
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

async fn loss_barrier(ws: &mut Client, events: &mut Vec<Value>) {
    let frame = r#"{"type":"round2.session.barrier","model":"round1-invariant-model","event_id":"barrier"}"#;
    send(ws, frame).await;
    loop {
        let event = recv(ws).await;
        if event["error"]["message"] == "unsupported websocket request type: round2.session.barrier"
        {
            break;
        }
        if event["error"]["event_id"] == "barrier" {
            assert!(
                matches!(
                    event["error"]["code"].as_str(),
                    Some("previous_response_not_found" | "upstream_websocket_not_sent")
                ),
                "{event}"
            );
            // The first barrier itself may have crossed the one loss epoch.
            send(ws, frame).await;
        } else {
            events.push(event);
        }
    }
}

fn loss_count(events: &[Value], id: &str) -> usize {
    events
        .iter()
        .filter(|event| {
            event["error"]["code"] == "upstream_websocket_lost"
                && event["error"]["event_id"].as_str().unwrap_or_default() == id
        })
        .count()
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
            let mut steered = false;
            let mut queued = false;
            let mut rx = rx.lock().await;
            loop {
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(Command::Begin(done)) => {
                            if !created { begin(&mut peer, "r1", "c1").await; created = true; }
                            let _ = done.send(());
                        }
                        Some(Command::Loss(done)) => { let _ = done.send(()); return; }
                        Some(Command::Quota(done)) => {
                            assert!(!created, "early 429 must precede accepted output");
                            peer.write(r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"early rejection","resets_in_seconds":120}}"#).await;
                            let _ = done.send(());
                        }
                        Some(Command::Terminal(done)) => {
                            terminal = true;
                            if !created { begin(&mut peer, "r1", "c1").await; created = true; }
                            if steered {
                                peer.write(r#"{"type":"response.incomplete","response":{"id":"r1","output":[]}}"#).await;
                                peer.write(r#"{"type":"response.created","response":{"id":"successor","previous_response_id":"r1"}}"#).await;
                            } else {
                                complete(&mut peer, "r1").await;
                                if queued { begin(&mut peer, "r2", "c2").await; complete(&mut peer, "r2").await; queued = false; }
                            }
                            let _ = done.send(());
                        }
                        None => return,
                    },
                    frame = peer.read() => {
                        let Some(frame) = frame else { return; };
                        match at(&frame, "/type").as_str() {
                            "response.steer" => {
                                steered = true;
                                peer.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#).await;
                            }
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

async fn schedule(steps: &[Step], duplex: bool) {
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
    let (mut steered, mut continued) = (false, false);
    for &step in steps {
        if closed {
            continue;
        }
        let commands = if retried {
            &sibling_commands
        } else {
            &first_commands
        };
        match step {
            Step::Early429 if !lost && !terminal && !steered => {
                let (done, complete) = oneshot::channel();
                first_commands.send(Command::Quota(done)).await.unwrap();
                complete.await.unwrap();
                loop {
                    let event = recv(&mut ws).await;
                    let ready = event["delta"] == "c1";
                    events.push(event);
                    if ready {
                        break;
                    }
                }
                retried = true;
                first.finished().await;
            }
            Step::Loss if !lost => {
                let (done, complete) = oneshot::channel();
                commands.send(Command::Loss(done)).await.unwrap();
                complete.await.unwrap();
                if retried {
                    sibling.finished().await;
                } else {
                    first.finished().await;
                }
                lost = true;
                loss_barrier(&mut ws, &mut events).await;
                assert_eq!(
                    loss_count(&events, "c1"),
                    usize::from(!terminal),
                    "{steps:?}: {ROUND1_INVARIANT}: {events:?}"
                );
                assert_eq!(
                    loss_count(&events, "op1"),
                    usize::from(steered),
                    "{steps:?}: {events:?}"
                );
                assert_eq!(
                    loss_count(&events, ""),
                    usize::from(steered && terminal),
                    "{steps:?}: {events:?}"
                );
                if continued {
                    let answers = events
                        .iter()
                        .filter(|event| {
                            event["error"]["event_id"] == "c2"
                                || event["type"] == "response.completed"
                                    && event["response"]["id"] == "r2"
                        })
                        .count();
                    assert_eq!(
                        answers, 1,
                        "queued continuation must receive one final answer: {steps:?}: {events:?}"
                    );
                }
            }
            Step::Terminal if !lost => {
                let (done, complete) = oneshot::channel();
                commands.send(Command::Terminal(done)).await.unwrap();
                complete.await.unwrap();
                loop {
                    let event = recv(&mut ws).await;
                    let ready = if steered {
                        event["type"] == "response.created"
                            && event["response"]["id"] == "successor"
                    } else {
                        event["type"] == "response.completed" && event["response"]["id"] == "r1"
                    };
                    events.push(event);
                    if ready {
                        break;
                    }
                }
                terminal = true;
            }
            Step::Steer if duplex && !lost && !terminal && !continued => {
                let (done, complete) = oneshot::channel();
                commands.send(Command::Begin(done)).await.unwrap();
                complete.await.unwrap();
                if !retried {
                    loop {
                        let event = recv(&mut ws).await;
                        let ready = event["delta"] == "c1";
                        events.push(event);
                        if ready {
                            break;
                        }
                    }
                }
                send(
                    &mut ws,
                    r#"{"type":"response.steer","event_id":"op1","input":[]}"#,
                )
                .await;
                loop {
                    let event = recv(&mut ws).await;
                    let ready = event["type"] == "response.steer.accepted";
                    events.push(event);
                    if ready {
                        break;
                    }
                }
                steered = true;
            }
            Step::Interrupt => {
                send(
                    &mut ws,
                    r#"{"type":"response.interrupt","response_id":"r1"}"#,
                )
                .await;
                if !lost && !terminal {
                    tokio::time::timeout(CLIENT_WAIT, interrupted.notified())
                        .await
                        .expect(ROUND1_INVARIANT);
                }
            }
            Step::Continuation => {
                send(&mut ws, r#"{"type":"response.create","model":"round1-invariant-model","input":[],"previous_response_id":"r1","event_id":"c2"}"#).await;
                continued = true;
            }
            Step::Disconnect => {
                if lost {
                    loss_barrier(&mut ws, &mut events).await;
                } else {
                    barrier(&mut ws, &mut events).await;
                }
                ws.close(None).await.unwrap();
                closed = true;
            }
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
    for id in ["", "c1", "c2", "op1"] {
        assert!(
            loss_count(&events, id) <= 1,
            "duplicate loss {id}: {context}: {events:?}"
        );
    }
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
            schedule(shape, duplex).await;
        }
    }
    let mut steered = Vec::new();
    permutations(
        &mut [
            Step::Loss,
            Step::Early429,
            Step::Interrupt,
            Step::Terminal,
            Step::Disconnect,
            Step::Continuation,
            Step::Steer,
        ],
        0,
        &mut steered,
    );
    let mut meaningful = 0;
    for shape in steered {
        let position = |wanted: Step| {
            shape
                .iter()
                .position(|step| std::mem::discriminant(step) == std::mem::discriminant(&wanted))
                .unwrap()
        };
        if [
            Step::Loss,
            Step::Terminal,
            Step::Disconnect,
            Step::Continuation,
        ]
        .iter()
        .all(|&step| position(Step::Steer) < position(step))
        {
            schedule(&shape, true).await;
            meaningful += 1;
        }
    }
    assert_eq!(meaningful, 1008);
}
