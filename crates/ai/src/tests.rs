//! The agent loop and image recognition against a fake OpenAI-compatible server on loopback.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cadcraft_engine::Session;
use serde_json::{Value, json};

use crate::{Agent, AiError, Backend, Config, Event, Image, Message, Vision, call_tool_with, recognize_image_markups};

/// An engine session answering the few control methods these tests use.
struct Sess(Session);

impl Backend for Sess {
    fn call(&mut self, method: &str, p: Value) -> Result<Value, String> {
        match method {
            "cmdline.input" => {
                self.0.cmdline(p["text"].as_str().unwrap_or_default()).map_err(|e| e.to_string())?;
                Ok(json!({"prompt": self.0.prompt_text()}))
            }
            "engine.execute" => self.0.execute(p["command"].as_str().unwrap_or_default(), &p["params"]).map_err(|e| e.to_string()),
            other => Err(format!("unknown method `{other}`")),
        }
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "test session".into()
    }
}

/// Serves one canned reply (status, JSON body) per connection, in order; returns the raw requests.
fn serve(replies: Vec<(u16, Vec<u8>)>) -> (Config, JoinHandle<Vec<String>>) {
    serve_chunks(replies.into_iter().map(|(s, b)| (s, "application/json", vec![b])).collect())
}

/// Like [`serve`], with a content type and a body sent in pieces with pauses between (an SSE stream).
fn serve_chunks(replies: Vec<(u16, &'static str, Vec<Vec<u8>>)>) -> (Config, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let cfg = Config {
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        model: "test-model".into(),
        api_key: "sekret".into(),
        vision: Vision::Auto,
        idle_timeout_ms: 0,
    };
    let h = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, ctype, chunks) in replies {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut conn = loop {
                match listener.accept() {
                    Ok((c, _)) => break c,
                    Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => return requests,
                }
            };
            conn.set_nonblocking(false).unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let (head_end, len) = loop {
                let n = conn.read(&mut buf).unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    break (i + 4, len);
                }
                if n == 0 {
                    break (raw.len(), 0);
                }
            };
            while raw.len() < head_end + len {
                let n = conn.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
            }
            requests.push(String::from_utf8_lossy(&raw).to_string());
            let _ = if ctype == "text/event-stream" {
                // No Content-Length: the stream ends when the connection closes.
                conn.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n").as_bytes())
            } else {
                let len: usize = chunks.iter().map(Vec::len).sum();
                conn.write_all(
                    format!("HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n").as_bytes(),
                )
            };
            for chunk in chunks {
                let _ = conn.write_all(&chunk);
                let _ = conn.flush();
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        requests
    });
    (cfg, h)
}

fn reply(message: Value) -> (u16, Vec<u8>) {
    (200, json!({"choices": [{"message": message}]}).to_string().into_bytes())
}

fn tool_call(id: &str, name: &str, args: &str) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": args}}]})
}

#[test]
fn the_agent_draws_a_circle_through_tool_calls() {
    let (cfg, server) = serve(vec![
        reply(tool_call("c1", "command_line", r#"{"text":"circle 0,0 5"}"#)),
        reply(json!({"role": "assistant", "content": "Drew a circle."})),
    ]);
    let mut backend = Sess(Session::new());
    let mut agent = Agent::new(cfg);
    let mut live = 0;
    let events = agent.send(&mut backend, "draw a circle of radius 5 at the origin", &AtomicBool::new(false), &mut |_| live += 1);
    assert_eq!(live, events.len());
    assert!(matches!(&events[0], Event::ToolCall { name, .. } if name == "command_line"), "{events:?}");
    assert!(matches!(&events[1], Event::ToolResult { error: false, .. }), "{events:?}");
    assert_eq!(events[2], Event::Text("Drew a circle.".into()));
    assert_eq!(events.len(), 3);
    let found = backend.0.execute("entities", &json!({"type": "Circle"})).unwrap();
    assert_eq!(found["count"], 1);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].to_ascii_lowercase().contains("authorization: bearer sekret"));
    assert!(requests[0].contains("POST /v1/chat/completions"));
    // The second request carries the tool's answer back.
    assert!(requests[1].contains(r#""tool_call_id":"c1""#), "{}", requests[1]);
}

#[test]
fn bad_tool_calls_come_back_to_the_model_as_errors_and_a_stuck_model_stops() {
    let (cfg, server) = serve(vec![
        reply(tool_call("a", "nonexistent", "{}")),
        reply(tool_call("b", "command_line", "not json")),
        reply(tool_call("c", "command_line", r#"{"text":5}"#)),
    ]);
    let mut agent = Agent::new(cfg);
    agent.max_steps = 3;
    let events = agent.send(&mut Sess(Session::new()), "go", &AtomicBool::new(false), &mut |_| {});
    let errors = events.iter().filter(|e| matches!(e, Event::ToolResult { error: true, .. })).count();
    assert_eq!(errors, 3, "{events:?}");
    assert!(matches!(events.last(), Some(Event::Error(m)) if m.contains("3 steps")), "{events:?}");
    server.join().unwrap();
}

#[test]
fn stop_is_honoured_before_the_next_request() {
    let (cfg, _server) = serve(vec![]);
    let events = Agent::new(cfg).send(&mut Sess(Session::new()), "go", &AtomicBool::new(true), &mut |_| {});
    assert_eq!(events, vec![Event::Error("stopped".into())]);
}

#[test]
fn a_server_that_goes_silent_ends_the_request_with_a_clear_error() {
    // Accepts the connection, reads the request and then says nothing at all.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let cfg =
        Config { base_url: format!("http://{}/v1", listener.local_addr().unwrap()), model: "m".into(), idle_timeout_ms: 300, ..Config::default() };
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else { return };
        let mut buf = [0u8; 8192];
        let _ = conn.read(&mut buf);
        let _ = done_rx.recv_timeout(Duration::from_secs(10));
    });
    let start = Instant::now();
    let events = Agent::new(cfg).send(&mut Sess(Session::new()), "go", &AtomicBool::new(false), &mut |_| {});
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    assert!(matches!(events.as_slice(), [Event::Error(m)] if m.contains("went silent")), "{events:?}");
    drop(done_tx);
    server.join().unwrap();
}

#[test]
fn garbage_oversized_and_failing_endpoints_are_errors_not_panics() {
    let cases: Vec<(u16, Vec<u8>)> = vec![
        (200, b"<html>not json</html>".to_vec()),
        (200, vec![0xff, 0xfe, 0x00]),
        (200, b"{\"choices\":[]}".to_vec()),
        (401, b"{\"error\":{\"message\":\"bad key\"}}".to_vec()),
        (500, Vec::new()),
        (200, vec![b' '; 5 << 20]),
    ];
    for (status, body) in cases {
        let (cfg, server) = serve(vec![(status, body)]);
        let events = Agent::new(cfg).send(&mut Sess(Session::new()), "go", &AtomicBool::new(false), &mut |_| {});
        assert!(matches!(events.as_slice(), [Event::Error(_)]), "{status}: {events:?}");
        server.join().unwrap();
    }
    // Nothing listens there.
    let cfg = Config { base_url: "http://127.0.0.1:1/v1".into(), model: "m".into(), ..Config::default() };
    let events = Agent::new(cfg).send(&mut Sess(Session::new()), "go", &AtomicBool::new(false), &mut |_| {});
    assert!(matches!(events.as_slice(), [Event::Error(_)]), "{events:?}");
}

#[test]
fn image_recognition_reads_the_models_json_and_drops_invalid_items() {
    let answer = "```json\n[{\"kind\":\"text\",\"text\":\"widen door\",\"at\":[0.2,0.3],\"height\":0.02},{\"kind\":\"cloud\",\"points\":[[2,2],[3,3]]},{\"kind\":\"rect\",\"points\":[[0.1,0.1],[0.4,0.3]]}]\n```";
    let (cfg, server) = serve(vec![reply(json!({"role": "assistant", "content": answer}))]);
    let m = recognize_image_markups(&cfg, b"\x89PNG fake", "image/png").unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!((m[0]["kind"].as_str(), m[1]["kind"].as_str()), (Some("text"), Some("rect")));
    let req = &server.join().unwrap()[0];
    assert!(req.contains("data:image/png;base64,"), "{req}");
    let (cfg, _s) = serve(vec![reply(json!({"role": "assistant", "content": "I see a drawing."}))]);
    assert!(matches!(recognize_image_markups(&cfg, b"x", "image/png"), Err(AiError::Reply(_))));
}

/// Records the engine calls it gets.
struct Recorder(Vec<(String, Value)>);

impl Backend for Recorder {
    fn call(&mut self, method: &str, p: Value) -> Result<Value, String> {
        self.0.push((method.into(), p));
        Ok(json!({"count": 0}))
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        String::new()
    }
}

#[test]
fn markup_import_sends_a_pdf_straight_on_and_reads_an_image_first() {
    let dir = std::env::temp_dir().join(format!("cadcraft-ai-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config { base_url: "http://127.0.0.1:1/v1".into(), model: "m".into(), ..Config::default() };
    let mut rec = Recorder(Vec::new());
    let pdf = dir.join("review.pdf").to_string_lossy().to_string();
    let r = call_tool_with(&mut rec, &cfg, "markup_import", &json!({"path": pdf, "page": 2, "at": [0, 0]})).unwrap();
    assert!(r.get("isError").is_none(), "{r}");
    assert_eq!(rec.0[0].0, "engine.execute");
    assert_eq!(rec.0[0].1["command"], "markupimport");
    assert_eq!(rec.0[0].1["params"]["page"], 2);
    assert!(rec.0[0].1["params"].get("markups").is_none());

    let png = dir.join("scan.png");
    std::fs::write(&png, b"\x89PNG fake").unwrap();
    let answer = r#"[{"kind":"polyline","points":[[0.1,0.1],[0.5,0.5]]}]"#;
    let (cfg, server) = serve(vec![reply(json!({"role": "assistant", "content": answer}))]);
    let r = call_tool_with(&mut rec, &cfg, "markup_import", &json!({"path": png.to_string_lossy(), "width": 100})).unwrap();
    assert!(r.get("isError").is_none(), "{r}");
    assert_eq!(rec.0[1].1["params"]["markups"][0]["kind"], "polyline");
    server.join().unwrap();

    // A missing image or an unreachable endpoint is a tool error, and nothing reaches the engine.
    let n = rec.0.len();
    let r = call_tool_with(&mut rec, &cfg, "markup_import", &json!({"path": dir.join("gone.png").to_string_lossy()})).unwrap();
    assert_eq!(r["isError"], true);
    assert_eq!(rec.0.len(), n);
    let _ = std::fs::remove_dir_all(&dir);
}

fn sse(deltas: &[Value]) -> Vec<u8> {
    deltas
        .iter()
        .map(|d| format!("data: {}\n\n", json!({"choices": [{"delta": d}]})))
        .chain(["data: [DONE]\n\n".to_string()])
        .collect::<String>()
        .into_bytes()
}

fn text_of(events: &[Event]) -> String {
    events.iter().filter_map(|e| if let Event::Text(t) = e { Some(t.as_str()) } else { None }).collect()
}

/// An agent that has already been shown a picture.
fn agent_with_picture(cfg: Config) -> Agent {
    let mut agent = Agent::new(cfg);
    agent.messages.push(Message::User { text: "look".into(), images: vec![Image { mime: "image/png".into(), base64: "QUJD".into() }] });
    agent
}

fn run(agent: &mut Agent, backend: &mut Sess) -> Vec<Event> {
    agent.send(backend, "go", &AtomicBool::new(false), &mut |_| {})
}

#[test]
fn a_streamed_answer_with_a_split_tool_call_draws_the_circle() {
    let first = sse(&[
        json!({"role": "assistant", "content": "Dra"}),
        json!({"content": "wing."}),
        json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "command_line", "arguments": ""}}]}),
        json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"text\":\"cir"}}]}),
        json!({"tool_calls": [{"index": 0, "function": {"arguments": "cle 0,0 5\"}"}}]}),
    ]);
    // The bytes arrive in pieces that cut lines in half.
    let cut = |b: &[u8]| b.chunks(37).map(<[u8]>::to_vec).collect::<Vec<_>>();
    let second = sse(&[json!({"content": "Done"}), json!({"content": "!"})]);
    let (cfg, server) = serve_chunks(vec![(200, "text/event-stream", cut(&first)), (200, "text/event-stream", cut(&second))]);
    let mut backend = Sess(Session::new());
    let events = run(&mut Agent::new(cfg), &mut backend);
    assert_eq!(text_of(&events), "Drawing.Done!", "{events:?}");
    assert!(matches!(&events[0], Event::Text(t) if t == "Dra"), "text shows while it arrives: {events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::ToolResult { error: false, .. })), "{events:?}");
    assert_eq!(backend.0.execute("entities", &json!({"type": "Circle"})).unwrap()["count"], 1);
    let requests = server.join().unwrap();
    assert!(requests[0].contains(r#""stream":true"#), "{}", requests[0]);
    assert!(requests[1].contains(r#""tool_call_id":"c1""#));
}

#[test]
fn a_400_about_images_retries_once_without_them_and_the_session_remembers() {
    let err = || (400, br#"{"error":{"message":"image input is not supported by this model"}}"#.to_vec());
    let (cfg, server) = serve(vec![err(), reply(json!({"content": "No picture seen."})), reply(json!({"content": "Still text only."}))]);
    let mut agent = agent_with_picture(cfg);
    let mut backend = Sess(Session::new());
    let events = run(&mut agent, &mut backend);
    assert!(matches!(&events[0], Event::Note(_)), "{events:?}");
    assert_eq!(text_of(&events), "No picture seen.");
    assert!(!events.iter().any(|e| matches!(e, Event::Error(_))), "{events:?}");
    assert!(agent.no_vision);
    // The next picture is not even tried.
    agent.messages.push(Message::User { text: "again".into(), images: vec![Image { mime: "image/png".into(), base64: "REVG".into() }] });
    assert_eq!(text_of(&run(&mut agent, &mut backend)), "Still text only.");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("image_url"));
    assert!(!requests[1].contains("image_url") && requests[1].contains("left out"), "{}", requests[1]);
    assert!(!requests[2].contains("image_url") && !requests[2].contains("REVG"));
}

#[test]
fn only_auto_retries_and_only_for_image_errors() {
    // An unrelated 400 is the error it is, with one request.
    let (cfg, server) = serve(vec![(400, br#"{"error":{"message":"unknown model"}}"#.to_vec())]);
    let events = run(&mut agent_with_picture(cfg), &mut Sess(Session::new()));
    assert!(matches!(events.as_slice(), [Event::Error(m)] if m.contains("unknown model")), "{events:?}");
    assert_eq!(server.join().unwrap().len(), 1);
    // Vision On insists: the refusal is the error.
    let (mut cfg, server) = serve(vec![(400, br#"{"error":{"message":"no multimodal input"}}"#.to_vec())]);
    cfg.vision = Vision::On;
    let events = run(&mut agent_with_picture(cfg), &mut Sess(Session::new()));
    assert!(matches!(events.as_slice(), [Event::Error(_)]), "{events:?}");
    assert_eq!(server.join().unwrap().len(), 1);
    // A refusal that stays a refusal after the retry ends the turn (one retry only).
    let img = || (400, br#"{"error":"bad image"}"#.to_vec());
    let (cfg, server) = serve(vec![img(), img()]);
    let events = run(&mut agent_with_picture(cfg), &mut Sess(Session::new()));
    assert!(matches!(events.last(), Some(Event::Error(_))), "{events:?}");
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn vision_off_sends_no_image_parts() {
    let (mut cfg, server) = serve(vec![reply(json!({"content": "ok"}))]);
    cfg.vision = Vision::Off;
    let events = run(&mut agent_with_picture(cfg), &mut Sess(Session::new()));
    assert_eq!(text_of(&events), "ok");
    let req = &server.join().unwrap()[0];
    assert!(!req.contains("image_url") && !req.contains("QUJD") && req.contains("left out"), "{req}");
    // Image recognition has nothing to work with.
    let cfg = Config { vision: Vision::Off, ..Config::default() };
    assert!(matches!(recognize_image_markups(&cfg, b"x", "image/png"), Err(AiError::Config(m)) if m.contains("vision")));
}

#[test]
fn a_server_that_rejects_streaming_is_asked_again_without_it() {
    let (cfg, server) = serve(vec![(400, br#"{"error":{"message":"streaming is not supported"}}"#.to_vec()), reply(json!({"content": "whole"}))]);
    let mut agent = Agent::new(cfg);
    let events = run(&mut agent, &mut Sess(Session::new()));
    assert_eq!(text_of(&events), "whole");
    assert!(agent.no_stream);
    let requests = server.join().unwrap();
    assert!(requests[0].contains(r#""stream":true"#) && !requests[1].contains(r#""stream""#), "{requests:?}");
}

#[test]
fn garbage_and_oversized_streams_are_errors_not_panics() {
    let line = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n", "x".repeat(1000));
    let huge = line.repeat(9 << 10).into_bytes();
    let cases: Vec<Vec<u8>> = vec![
        b"data: not json\n\n".to_vec(),
        b": only a comment\n\n".to_vec(),
        b"data: [DONE]\n\n".to_vec(),
        b"data: {\"error\":{\"message\":\"overloaded\"}}\n\n".to_vec(),
        vec![b'd'; 2 << 20],
        vec![0xff, 0xfe, b'\n'],
        huge,
    ];
    for body in cases {
        let (cfg, server) = serve_chunks(vec![(200, "text/event-stream", vec![body])]);
        let events = run(&mut Agent::new(cfg), &mut Sess(Session::new()));
        assert!(matches!(events.last(), Some(Event::Error(_))), "{events:?}");
        server.join().unwrap();
    }
}

#[test]
fn a_driver_polls_without_blocking_and_stops_on_cancel() {
    use std::sync::Arc;
    // The first line, then comments for a few seconds: the reply stays open.
    let mut chunks = vec![b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n".to_vec()];
    chunks.extend(std::iter::repeat_n(b": ping\n".to_vec(), 300));
    let (cfg, _server) = serve_chunks(vec![(200, "text/event-stream", chunks)]);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut d = crate::Driver::start(Agent::new(cfg), "go", false, cancel.clone(), Arc::new(|| {}));
    let mut backend = Sess(Session::new());
    // The reply is still open: poll returns at once, again and again.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while !events.iter().any(|e| matches!(e, Event::Text(_))) && Instant::now() < deadline {
        assert!(d.poll(&mut backend));
        events.extend(d.take_events());
        std::thread::sleep(Duration::from_millis(5));
    }
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(!d.poll(&mut backend));
    events.extend(d.take_events());
    assert_eq!(events.last(), Some(&Event::Error("stopped".into())), "{events:?}");
}
