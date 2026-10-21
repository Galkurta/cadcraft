//! The Assistant palette: ASSISTANTOPEN docks it, a message runs the model's tool calls on the UI
//! thread against the drawing (here a fake OpenAI-compatible server on loopback), the answer shows
//! while it streams in, tool calls are one collapsible line, and the endpoint settings are saved
//! with the preferences while the API key goes to the credential store (a fake one here: the real
//! store is never touched).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cadcraft_ai::keystore::{ApiKey, KeyStore};
use cadcraft_engine::Session;
use cadcraft_ui_egui::{CadApp, Services, docking::Panel};
use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

/// A credential store in memory (`broken`: one that is never available).
#[derive(Clone, Default)]
struct Mem {
    secret: Arc<Mutex<Option<String>>>,
    broken: bool,
}

impl KeyStore for Mem {
    fn get(&self) -> Result<Option<String>, String> {
        if self.broken { Err("no secret service".into()) } else { Ok(self.secret.lock().unwrap().clone()) }
    }
    fn set(&self, key: &str) -> Result<(), String> {
        if self.broken {
            return Err("no secret service".into());
        }
        *self.secret.lock().unwrap() = Some(key.into());
        Ok(())
    }
    fn delete(&self) -> Result<(), String> {
        if self.broken {
            return Err("no secret service".into());
        }
        *self.secret.lock().unwrap() = None;
        Ok(())
    }
}

/// An app whose key store is `store`.
fn app_with(store: &Mem) -> CadApp {
    let mut app = CadApp::new(Session::default(), Services::default());
    app.assistant.keys = ApiKey::new(Box::new(store.clone()));
    app
}

/// One connection's reply: a content type and a body in pieces, each after a pause (ms).
type Reply = (&'static str, Vec<(Vec<u8>, u64)>);

/// Serves one canned reply per connection, in order.
fn serve_raw(replies: Vec<Reply>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for (ctype, pieces) in replies {
            let Ok((mut conn, _)) = listener.accept() else { return };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = conn.read(&mut buf).unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                let Some(i) = text.find("\r\n\r\n") else {
                    if n == 0 {
                        break;
                    }
                    continue;
                };
                let len = text
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                if raw.len() >= i + 4 + len || n == 0 {
                    break;
                }
            }
            let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nConnection: close\r\n\r\n");
            for (bytes, pause) in pieces {
                std::thread::sleep(Duration::from_millis(pause));
                let _ = conn.write_all(&bytes);
                let _ = conn.flush();
            }
        }
    });
    url
}

/// Serves one JSON chat message per connection, in order.
fn serve(messages: Vec<Value>) -> String {
    serve_raw(messages.into_iter().map(|m| ("application/json", vec![(json!({"choices": [{"message": m}]}).to_string().into_bytes(), 0)])).collect())
}

fn sse_line(delta: Value) -> Vec<u8> {
    format!("data: {}\n\n", json!({"choices": [{"delta": delta}]})).into_bytes()
}

/// The Assistant's input: the one text field that is empty (the others show the drawing's coordinates).
fn assistant_input<'a>(h: &'a Harness<'a, CadApp>) -> egui_kittest::Node<'a> {
    h.query_all_by_role(egui::accesskit::Role::TextInput).find(|n| n.value().is_none_or(|v| v.is_empty())).unwrap()
}

fn circles(app: &mut CadApp) -> u64 {
    app.session.execute("entities", &json!({"type": "Circle"})).map(|v| v["count"].as_u64().unwrap_or(0)).unwrap_or(0)
}

/// The palette open in a window, ready for a message.
fn harness(mut app: CadApp, url: &str) -> Harness<'static, CadApp> {
    app.assistant.settings.base_url = url.into();
    app.assistant.settings.model = "m".into();
    assert!(app.cmdline("assistantopen").is_none());
    assert!(app.ui.docking.layout.contains(&Panel::Assistant));
    app.ui.in_window_menu = false;
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut CadApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h
}

fn ask(h: &mut Harness<'static, CadApp>, text: &str) {
    assistant_input(h).click();
    h.run_steps(2);
    h.event(egui::Event::Text(text.into()));
    h.run_steps(2);
    h.get_by_label("Send").click();
}

fn wait_until_done(h: &mut Harness<'static, CadApp>) {
    let start = Instant::now();
    loop {
        h.run_steps(1);
        if !h.state().assistant.running() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the Assistant did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(2);
}

#[test]
fn a_message_draws_through_the_models_tool_calls_and_the_settings_are_saved() {
    let url = serve(vec![
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "1", "type": "function", "function": {"name": "command_line", "arguments": "{\"text\":\"circle 0,0 5\"}"}}]}),
        json!({"role": "assistant", "content": "Drew the circle."}),
    ]);
    let store = Mem::default();
    let mut app = app_with(&store);
    app.assistant.settings.api_key = "key-123".into();
    let mut h = harness(app, &url);
    ask(&mut h, "draw a circle");
    wait_until_done(&mut h);
    assert_eq!(circles(h.state_mut()), 1);
    h.get_by_label("Drew the circle.");
    h.get_by_label("draw a circle");

    // The call is one line; its JSON opens on click.
    assert!(h.query_by_label_contains("\"text\"").is_none());
    h.get_by_label("command_line circle 0,0 5 — ok").click();
    h.run_steps(3);
    h.get_by_label_contains("\"text\": \"circle 0,0 5\"");

    // The key went to the credential store; the preferences keep the rest.
    assert_eq!(store.secret.lock().unwrap().as_deref(), Some("key-123"));
    let prefs = h.state().prefs_json();
    assert!(!prefs.contains("key-123") && prefs.contains("\"model\":\"m\""), "{prefs}");
    let mut restored = app_with(&store);
    restored.load_prefs(&prefs);
    assert_eq!((restored.assistant.settings.base_url.as_str(), restored.assistant.settings.model.as_str()), (url.as_str(), "m"));
    assert_eq!(restored.assistant.keys.load().as_deref(), Some("key-123"));
    // Garbage is ignored.
    restored.load_prefs(r#"{"assistant": 5}"#);
    restored.load_prefs(r#"{"assistant": {"baseUrl": 3, "model": null, "vision": "bogus"}}"#);
    assert_eq!(restored.assistant.settings.model, "m");
    let fresh = CadApp::new(Session::default(), Services::default());
    assert!(!fresh.prefs_json().contains("assistant"));

    // ASSISTANTCLOSE closes it.
    let app = h.state_mut();
    assert!(app.cmdline("assistantclose").is_none());
    assert!(!app.ui.docking.layout.contains(&Panel::Assistant));
}

#[test]
fn a_plain_text_key_in_old_preferences_moves_into_the_store_and_leaves_the_file() {
    let store = Mem::default();
    let mut app = app_with(&store);
    app.load_prefs(r#"{"assistant": {"model": "m", "apiKey": "old-plain"}}"#);
    assert_eq!(store.secret.lock().unwrap().as_deref(), Some("old-plain"));
    // Still usable in the session, gone from what is saved.
    assert_eq!(app.assistant.settings.api_key, "old-plain");
    let prefs = app.prefs_json();
    assert!(!prefs.contains("old-plain") && !prefs.contains("apiKey") && prefs.contains("\"model\":\"m\""), "{prefs}");
}

#[test]
fn without_a_credential_store_the_key_stays_in_the_preferences_with_a_warning() {
    let broken = Mem { broken: true, ..Mem::default() };
    let mut app = app_with(&broken);
    app.load_prefs(r#"{"assistant": {"apiKey": "old-plain"}}"#);
    assert!(app.prefs_json().contains("old-plain"));
    assert!(app.assistant.keys.warning().is_some_and(|w| w.contains("no secret service")));
    // A key typed later is kept in the preferences as well.
    let mut app = app_with(&broken);
    app.assistant.settings.api_key = "typed".into();
    app.assistant.keys.save("typed");
    assert!(app.prefs_json().contains("typed"));
}

#[test]
fn the_answer_shows_while_it_streams_and_a_long_one_is_cut_until_show_more() {
    let long = format!("{}END", "word ".repeat(200));
    let url = serve_raw(vec![
        (
            "text/event-stream",
            vec![(sse_line(json!({"content": "Hel"})), 0), (sse_line(json!({"content": "lo there"})), 800), (b"data: [DONE]\n\n".to_vec(), 0)],
        ),
        ("text/event-stream", vec![(sse_line(json!({"content": long.clone()})), 0), (b"data: [DONE]\n\n".to_vec(), 0)]),
    ]);
    let mut h = harness(app_with(&Mem::default()), &url);
    ask(&mut h, "hi");
    let start = Instant::now();
    let mut partial = false;
    loop {
        h.run_steps(1);
        if !h.state().assistant.running() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the Assistant did not finish");
        partial |= h.query_by_label("Hel").is_some();
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(2);
    assert!(partial, "the first words were never shown before the rest arrived");
    h.get_by_label("Hello there");

    ask(&mut h, "talk");
    wait_until_done(&mut h);
    assert!(h.query_by_label_contains("END").is_none(), "shown in full");
    h.get_by_label("Show more").click();
    h.run_steps(3);
    h.get_by_label_contains("END");
    h.get_by_label("Show less");
}

#[test]
fn stop_ends_a_streaming_request_at_once_and_later_chunks_are_ignored() {
    let url = serve_raw(vec![(
        "text/event-stream",
        vec![(sse_line(json!({"content": "Hel"})), 0), (sse_line(json!({"content": "LATE"})), 2500), (b"data: [DONE]\n\n".to_vec(), 0)],
    )]);
    let mut h = harness(app_with(&Mem::default()), &url);
    ask(&mut h, "hi");
    let start = Instant::now();
    while h.query_by_label("Hel").is_none() {
        h.run_steps(1);
        assert!(start.elapsed() < Duration::from_secs(20), "nothing streamed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let clicked = Instant::now();
    h.get_by_label("Stop").click();
    h.run_steps(2);
    assert!(!h.state().assistant.running(), "still running after Stop");
    assert!(clicked.elapsed() < Duration::from_millis(200), "{:?}", clicked.elapsed());
    h.get_by_label("Stopped");
    // The slow chunk arrives after Stop and must not show up.
    std::thread::sleep(Duration::from_millis(2800));
    h.run_steps(3);
    assert!(h.query_by_label_contains("LATE").is_none(), "a chunk after Stop was shown");
    assert!(!h.state().assistant.running());
}

#[test]
fn an_unreachable_endpoint_is_reported_in_the_transcript() {
    let mut h = harness(app_with(&Mem::default()), "http://127.0.0.1:1/v1");
    ask(&mut h, "hello");
    let start = Instant::now();
    loop {
        h.run_steps(1);
        if !h.state().assistant.running() && h.query_by_label_contains("can't reach").is_some() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "no error shown");
        std::thread::sleep(Duration::from_millis(10));
    }
}
