//! One HTTP POST whose reply arrives in pieces, the same on both platforms: native runs it on a
//! worker thread (blocking `ureq`), the web build on the browser's `fetch` (`spawn_local`). The
//! caller never blocks: it polls a [`Flight`] for the status and the bytes received so far.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;

use crate::{AiError, Config};

/// Called from the request's thread (or task) when something new arrived, to wake the UI.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// The reply is read up to this many bytes (a long streamed answer is far below it).
pub const MAX_REPLY_BYTES: usize = 8 << 20;
/// Local models can take minutes for a long answer; the whole exchange is cut off after this.
#[cfg(not(target_arch = "wasm32"))]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
#[cfg(target_arch = "wasm32")]
const TIMEOUT_MS: i32 = 300_000;
/// Without a byte from the server for this long the request ends with an error.
pub const DEFAULT_IDLE_MS: u64 = 60_000;

#[derive(Default)]
struct Shared {
    status: Option<u16>,
    data: Vec<u8>,
    total: usize,
    finished: Option<Result<(), String>>,
    cancelled: bool,
    /// Web: Stop aborts the `fetch` at once, not at its next chunk.
    #[cfg(target_arch = "wasm32")]
    abort: Option<web_sys::AbortController>,
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a [`Flight`] has received since the last [`Flight::drain`].
pub struct Drained {
    pub status: Option<u16>,
    pub data: Vec<u8>,
    /// `Some` once the exchange is over (`Err`: the transport failed).
    pub finished: Option<Result<(), String>>,
}

/// The caller's end of a request in flight.
pub struct Flight(Arc<Mutex<Shared>>);

impl Flight {
    pub fn drain(&self) -> Drained {
        let mut s = lock(&self.0);
        Drained { status: s.status, data: std::mem::take(&mut s.data), finished: s.finished.clone() }
    }

    /// Give up: the reader stops at once (web: the `fetch` is aborted; native: its watcher returns
    /// and ignores whatever the socket delivers later).
    pub fn cancel(&self) {
        let mut s = lock(&self.0);
        s.cancelled = true;
        #[cfg(target_arch = "wasm32")]
        if let Some(a) = &s.abort {
            a.abort();
        }
    }
}

/// The request's end: the thread or task that reads the reply.
struct Sink {
    shared: Arc<Mutex<Shared>>,
    wake: Wake,
}

impl Sink {
    fn cancelled(&self) -> bool {
        lock(&self.shared).cancelled
    }

    fn status(&self, status: u16) {
        lock(&self.shared).status = Some(status);
    }

    /// Hand over bytes; false when the reader should stop (cancelled or over the size cap).
    fn push(&self, bytes: &[u8]) -> bool {
        let mut s = lock(&self.shared);
        if s.cancelled {
            return false;
        }
        s.total = s.total.saturating_add(bytes.len());
        if s.total > MAX_REPLY_BYTES {
            s.finished.get_or_insert(Err(format!("the reply is larger than {} MB", MAX_REPLY_BYTES >> 20)));
            return false;
        }
        s.data.extend_from_slice(bytes);
        drop(s);
        (self.wake)();
        true
    }

    fn finish(self, result: Result<(), String>) {
        lock(&self.shared).finished.get_or_insert(result);
    }
}

impl Drop for Sink {
    /// However the reader ends (even by a panic), the caller hears of it.
    fn drop(&mut self) {
        lock(&self.shared).finished.get_or_insert(Err("the request stopped unexpectedly".into()));
        (self.wake)();
    }
}

/// The host of an `http://` base URL when `key` would travel to it unencrypted: `None` for https,
/// for loopback hosts (localhost, 127.0.0.0/8, ::1) and when no key is set. Userinfo, ports and
/// IPv6 brackets are parsed away, so `127.0.0.1.evil.com` and `localhost@evil.com` count as remote.
pub fn insecure_key_host(base_url: &str, key: &str) -> Option<String> {
    let url = base_url.trim();
    let rest = url.get(..7).filter(|s| s.eq_ignore_ascii_case("http://")).and_then(|_| url.get(7..))?;
    if key.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let hostport = authority.rsplit('@').next().unwrap_or_default();
    let host = match hostport.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => hostport.split(':').next().unwrap_or_default(),
    };
    let loopback = host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    (!loopback).then(|| host.to_string())
}

/// POST `body` to `{base_url}/chat/completions`. Returns at once; poll the [`Flight`]. `stream`:
/// the reply is an event stream, so a pause of [`DEFAULT_IDLE_MS`] means the server is stuck; a
/// one-piece reply may legitimately take minutes and gets the whole-exchange timeout instead.
pub fn start(cfg: &Config, body: &Value, stream: bool, wake: Wake) -> Result<Flight, AiError> {
    let base = cfg.base_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(AiError::Config(format!("the AI base URL `{base}` must start with http:// or https://")));
    }
    // The one place every request (native `ureq`, web `fetch`) passes through.
    if let Some(host) = insecure_key_host(base, &cfg.api_key) {
        return Err(AiError::Config(format!("refusing to send the API key over unencrypted http to {host}; use https or a loopback address")));
    }
    let shared = Arc::new(Mutex::new(Shared::default()));
    let sink = Sink { shared: shared.clone(), wake };
    let (url, key, body) = (format!("{base}/chat/completions"), cfg.api_key.clone(), body.to_string());
    let idle_ms = match cfg.idle_timeout_ms {
        0 if stream => DEFAULT_IDLE_MS,
        0 => 300_000,
        ms => ms,
    };
    let idle = std::time::Duration::from_millis(idle_ms);
    spawn(url, key, body, idle, sink)?;
    Ok(Flight(shared))
}

#[cfg(not(target_arch = "wasm32"))]
enum Msg {
    Status(u16),
    Chunk(Vec<u8>),
    Done(Result<(), String>),
}

/// Two threads: one blocks in `ureq`, the other watches it, so Stop and a silent server end the
/// request on time. ponytail: a blocked socket read can't be interrupted; the reader thread is
/// abandoned (its sends fail) and ends when the server answers or the global timeout hits.
#[cfg(not(target_arch = "wasm32"))]
fn spawn(url: String, key: String, body: String, idle: std::time::Duration, sink: Sink) -> Result<(), AiError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let err = |e: std::io::Error| AiError::Transport(e.to_string());
    std::thread::Builder::new()
        .name("ai-request".into())
        .spawn(move || {
            let r = read_native(&url, &key, body, &tx);
            let _ = tx.send(Msg::Done(r));
        })
        .map_err(err)?;
    std::thread::Builder::new().name("ai-watch".into()).spawn(move || watch(&rx, idle, sink)).map(drop).map_err(err)
}

#[cfg(not(target_arch = "wasm32"))]
fn watch(rx: &std::sync::mpsc::Receiver<Msg>, idle: std::time::Duration, sink: Sink) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut last = std::time::Instant::now();
    let result = loop {
        if sink.cancelled() {
            break Ok(());
        }
        match rx.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(Msg::Status(s)) => sink.status(s),
            Ok(Msg::Chunk(c)) => {
                if !sink.push(&c) {
                    break Ok(());
                }
            }
            Ok(Msg::Done(r)) => break r,
            Err(RecvTimeoutError::Timeout) => {
                if last.elapsed() >= idle {
                    break Err(format!("the server went silent: no data for {idle:?}"));
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break Err("the request stopped unexpectedly".into()),
        }
        last = std::time::Instant::now();
    };
    sink.finish(result);
}

#[cfg(not(target_arch = "wasm32"))]
fn read_native(url: &str, key: &str, body: String, tx: &std::sync::mpsc::Sender<Msg>) -> Result<(), String> {
    use std::io::Read;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.post(url).header("Content-Type", "application/json");
    if !key.is_empty() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let mut resp = req.send(body).map_err(|e| e.to_string())?;
    tx.send(Msg::Status(resp.status().as_u16())).map_err(|e| e.to_string())?;
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        match buf.get(..n) {
            // The watcher is gone (Stop, idle timeout): nobody wants more.
            Some(chunk) if n > 0 => tx.send(Msg::Chunk(chunk.to_vec())).map_err(|e| e.to_string())?,
            _ => return Ok(()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn spawn(url: String, key: String, body: String, idle: std::time::Duration, sink: Sink) -> Result<(), AiError> {
    // The controller is in the shared state first, so Stop aborts the fetch from the first moment.
    let abort = web_sys::AbortController::new().map_err(|e| AiError::Transport(js_err(e)))?;
    lock(&sink.shared).abort = Some(abort.clone());
    wasm_bindgen_futures::spawn_local(async move {
        let r = fetch(&url, &key, &body, idle, &abort, &sink).await;
        sink.finish(r);
    });
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn js_err(e: wasm_bindgen::JsValue) -> String {
    use wasm_bindgen::JsCast;
    let msg = e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())).or_else(|| e.as_string()).unwrap_or_else(|| "request failed".into());
    // `fetch` rejects with a bare TypeError for network errors and for CORS refusals alike.
    format!("{msg} (is the server reachable, and does it allow cross-origin requests from this page?)")
}

#[cfg(target_arch = "wasm32")]
async fn fetch(url: &str, key: &str, body: &str, idle: std::time::Duration, abort: &web_sys::AbortController, sink: &Sink) -> Result<(), String> {
    use std::{cell::Cell, rc::Rc};
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, ReadableStreamDefaultReader, Request, RequestInit, Response};

    let win = web_sys::window().ok_or("no browser window")?;
    // The whole exchange is cut off after TIMEOUT_MS, like the native client's global timeout.
    let on_timeout = Closure::once_into_js({
        let abort = abort.clone();
        move || abort.abort()
    });
    let timer = win.set_timeout_with_callback_and_timeout_and_arguments_0(on_timeout.unchecked_ref(), TIMEOUT_MS).ok();
    // A server that goes silent: the idle timer is re-armed with every chunk and aborts the fetch.
    let silent = Rc::new(Cell::new(false));
    let on_idle = Closure::<dyn FnMut()>::new({
        let (abort, silent) = (abort.clone(), silent.clone());
        move || {
            silent.set(true);
            abort.abort();
        }
    });
    let idle_ms = i32::try_from(idle.as_millis()).unwrap_or(i32::MAX);
    let arm = || win.set_timeout_with_callback_and_timeout_and_arguments_0(on_idle.as_ref().unchecked_ref(), idle_ms).ok();
    let mut idle_timer = arm();
    let result = async {
        let headers = Headers::new().map_err(js_err)?;
        headers.set("Content-Type", "application/json").map_err(js_err)?;
        if !key.is_empty() {
            headers.set("Authorization", &format!("Bearer {key}")).map_err(js_err)?;
        }
        let init = RequestInit::new();
        init.set_method("POST");
        init.set_headers(&headers);
        init.set_body(&JsValue::from_str(body));
        init.set_signal(Some(&abort.signal()));
        let req = Request::new_with_str_and_init(url, &init).map_err(js_err)?;
        let resp: Response = JsFuture::from(win.fetch_with_request(&req)).await.map_err(js_err)?.dyn_into().map_err(|_| "not a response")?;
        sink.status(resp.status());
        let Some(stream) = resp.body() else { return Ok(()) };
        let reader: ReadableStreamDefaultReader = stream.get_reader().dyn_into().map_err(|_| "no stream reader")?;
        loop {
            let chunk = JsFuture::from(reader.read()).await.map_err(js_err)?;
            if let Some(t) = idle_timer.take() {
                win.clear_timeout_with_handle(t);
            }
            let done = js_sys::Reflect::get(&chunk, &"done".into()).ok().and_then(|d| d.as_bool()).unwrap_or(true);
            if done {
                return Ok(());
            }
            let value = js_sys::Reflect::get(&chunk, &"value".into()).map_err(js_err)?;
            if !sink.push(&js_sys::Uint8Array::new(&value).to_vec()) {
                abort.abort();
                return Ok(());
            }
            idle_timer = arm();
        }
    }
    .await;
    if let Some(t) = timer {
        win.clear_timeout_with_handle(t);
    }
    if sink.cancelled() {
        return Ok(());
    }
    if silent.get() {
        return Err(format!("the server went silent: no data for {idle:?}"));
    }
    result
}
