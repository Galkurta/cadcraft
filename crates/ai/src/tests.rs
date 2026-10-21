//! The agent loop and image recognition against a fake OpenAI-compatible server on loopback.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cadcraft_engine::Session;
use serde_json::{Value, json};

use crate::{Agent, AiError, Backend, Config, Event, call_tool_with, recognize_image_markups};

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

/// Serves one canned reply (status, body) per connection, in order; returns the raw requests.
fn serve(replies: Vec<(u16, Vec<u8>)>) -> (Config, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let cfg = Config { base_url: format!("http://{}/v1", listener.local_addr().unwrap()), model: "test-model".into(), api_key: "sekret".into() };
    let h = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in replies {
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
            let head =
                format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(&body);
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
    let cfg = Config { base_url: "http://127.0.0.1:1/v1".into(), model: "m".into(), api_key: String::new() };
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
    let cfg = Config { base_url: "http://127.0.0.1:1/v1".into(), model: "m".into(), api_key: String::new() };
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
