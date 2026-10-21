//! The Assistant palette: ASSISTANTOPEN docks it, a message runs the model's tool calls on the UI
//! thread against the drawing (here a fake OpenAI-compatible server on loopback), and the
//! endpoint settings are saved with the preferences.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use cadcraft_engine::Session;
use cadcraft_ui_egui::{CadApp, Services, docking::Panel};
use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

/// Serves one canned message per connection, in order.
fn serve(messages: Vec<Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for m in messages {
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
            let body = json!({"choices": [{"message": m}]}).to_string();
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

/// The Assistant's input: the one text field that is empty (the others show the drawing's coordinates).
fn assistant_input<'a>(h: &'a Harness<'a, CadApp>) -> egui_kittest::Node<'a> {
    h.query_all_by_role(egui::accesskit::Role::TextInput).find(|n| n.value().is_none_or(|v| v.is_empty())).unwrap()
}

fn circles(app: &mut CadApp) -> u64 {
    app.session.execute("entities", &json!({"type": "Circle"})).map(|v| v["count"].as_u64().unwrap_or(0)).unwrap_or(0)
}

#[test]
fn a_message_draws_through_the_models_tool_calls_and_the_settings_are_saved() {
    let url = serve(vec![
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "1", "type": "function", "function": {"name": "command_line", "arguments": "{\"text\":\"circle 0,0 5\"}"}}]}),
        json!({"role": "assistant", "content": "Drew the circle."}),
    ]);
    let mut app = CadApp::new(Session::default(), Services::default());
    app.assistant.settings.base_url = url;
    app.assistant.settings.model = "m".into();
    app.assistant.settings.api_key = "key-123".into();
    // The typed command opens the palette.
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
    assistant_input(&h).click();
    h.run_steps(2);
    h.event(egui::Event::Text("draw a circle".into()));
    h.run_steps(2);
    h.get_by_label("Send").click();
    let start = Instant::now();
    while circles(h.state_mut()) == 0 || h.state().assistant.running() {
        assert!(start.elapsed() < Duration::from_secs(20), "the Assistant did not finish");
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(2);
    assert_eq!(circles(h.state_mut()), 1);
    h.get_by_label("Drew the circle.");
    h.get_by_label("draw a circle");

    // Saved with the preferences, restored from them; garbage is ignored.
    let prefs = h.state().prefs_json();
    assert!(prefs.contains("key-123"), "{prefs}");
    let mut restored = CadApp::new(Session::default(), Services::default());
    restored.load_prefs(&prefs);
    assert_eq!(restored.assistant.settings, h.state().assistant.settings);
    restored.load_prefs(r#"{"assistant": 5}"#);
    restored.load_prefs(r#"{"assistant": {"baseUrl": 3, "model": null}}"#);
    assert_eq!(restored.assistant.settings, h.state().assistant.settings);
    let fresh = CadApp::new(Session::default(), Services::default());
    assert!(!fresh.prefs_json().contains("assistant"));

    // ASSISTANTCLOSE closes it.
    let app = h.state_mut();
    assert!(app.cmdline("assistantclose").is_none());
    assert!(!app.ui.docking.layout.contains(&Panel::Assistant));
}

#[test]
fn an_unreachable_endpoint_is_reported_in_the_transcript() {
    let mut app = CadApp::new(Session::default(), Services::default());
    app.assistant.settings.base_url = "http://127.0.0.1:1/v1".into();
    app.assistant.settings.model = "m".into();
    app.cmdline("assistantopen");
    app.ui.in_window_menu = false;
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut CadApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    assistant_input(&h).click();
    h.run_steps(2);
    h.event(egui::Event::Text("hello".into()));
    h.run_steps(2);
    h.get_by_label("Send").click();
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
