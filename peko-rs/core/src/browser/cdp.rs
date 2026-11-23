//! A small DevTools-protocol client (ADR-119 D1).
//!
//! The browser is driven over a pipe it inherits (`--remote-debugging-pipe`:
//! JSON messages ending in a NUL byte, read on fd 3 and written on fd 4),
//! so no other process on the computer can reach it. Where a pipe cannot
//! be passed (Windows), it is driven over a WebSocket on a loopback port
//! the browser picks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};

/// How long one protocol call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Something the browser reported unasked.
#[derive(Debug)]
pub struct Event {
    /// The tab session it belongs to; `None` for the browser itself.
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// One connection to a browser.
pub struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<Arc<Event>>,
}

impl std::fmt::Debug for Cdp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cdp").finish_non_exhaustive()
    }
}

impl Cdp {
    fn new(
        out: mpsc::UnboundedSender<String>,
    ) -> (Arc<Self>, Pending, broadcast::Sender<Arc<Event>>) {
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(256);
        let cdp = Arc::new(Self {
            out,
            pending: Arc::clone(&pending),
            next_id: AtomicU64::new(1),
            events: events.clone(),
        });
        (cdp, pending, events)
    }

    /// Call `method`; `session` names a tab, `None` the browser.
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if self.out.send(message.to_string()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(anyhow!("the browser has stopped"));
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(anyhow!("the browser has stopped")),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(anyhow!("the browser did not answer `{method}` in time"))
            }
        }
    }

    /// Listen to what the browser reports.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }
}

/// Route one message from the browser: an answer to its caller, anything
/// else to the listeners.
fn route(text: &str, pending: &Pending, events: &broadcast::Sender<Arc<Event>>) {
    let Ok(mut message) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        let Some(tx) = pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            return;
        };
        let result = match message.get("error") {
            Some(error) => Err(anyhow!(
                "{}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the browser refused the call")
            )),
            None => Ok(message
                .get_mut("result")
                .map(Value::take)
                .unwrap_or(Value::Null)),
        };
        let _ = tx.send(result);
        return;
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    let _ = events.send(Arc::new(Event {
        session: message
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        method: method.to_string(),
        params: message
            .get_mut("params")
            .map(Value::take)
            .unwrap_or(Value::Null),
    }));
}

/// Fail every caller still waiting: the connection is gone.
fn drain(pending: &Pending) {
    pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Drive a browser over the two halves of its debugging pipe.
pub fn over_pipe<R, W>(mut read: R, mut write: W) -> Arc<Cdp>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
    let (cdp, pending, events) = Cdp::new(out);
    tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            let mut bytes = message.into_bytes();
            bytes.push(0);
            if write.write_all(&bytes).await.is_err() || write.flush().await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match read.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
            while let Some(end) = buffer.iter().position(|b| *b == 0) {
                let message: Vec<u8> = buffer.drain(..=end).collect();
                if let Ok(text) = std::str::from_utf8(&message[..message.len() - 1]) {
                    route(text, &pending, &events);
                }
            }
        }
        drain(&pending);
    });
    cdp
}

/// Drive a browser over its WebSocket at `url` (`ws://127.0.0.1:<port>/devtools/browser/<id>`).
pub async fn over_websocket(url: &str) -> Result<Arc<Cdp>> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    use tokio_tungstenite::tungstenite::Message;

    // Frames of a page are large messages.
    let config = WebSocketConfig::default()
        .max_message_size(Some(64 << 20))
        .max_frame_size(Some(64 << 20));
    let (socket, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false)
        .await
        .with_context(|| format!("connect to the browser at {url}"))?;
    let (mut sink, mut stream) = socket.split();
    let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
    let (cdp, pending, events) = Cdp::new(out);
    tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            if sink.send(Message::text(message)).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            if let Message::Text(text) = message {
                route(text.as_str(), &pending, &events);
            }
        }
        drain(&pending);
    });
    Ok(cdp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_reach_their_caller_and_the_rest_are_events() {
        let (ours, theirs) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(ours);
        let cdp = over_pipe(read, write);
        let mut events = cdp.subscribe();

        // A stand-in browser: answers each call with its own params, and
        // reports one event first.
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let (read, mut write) = tokio::io::split(theirs);
            let mut read = BufReader::new(read);
            loop {
                let mut raw = Vec::new();
                if read.read_until(0, &mut raw).await.unwrap_or(0) == 0 {
                    break;
                }
                raw.pop();
                let call: Value = serde_json::from_slice(&raw).unwrap();
                let event =
                    json!({"method":"Page.loadEventFired","sessionId":"s1","params":{"t":1}});
                let answer = if call["method"] == "Bad.call" {
                    json!({"id": call["id"], "error": {"message": "no such method"}})
                } else {
                    json!({"id": call["id"], "result": call["params"]})
                };
                for message in [event, answer] {
                    let mut bytes = message.to_string().into_bytes();
                    bytes.push(0);
                    write.write_all(&bytes).await.unwrap();
                }
            }
        });

        let result = cdp
            .call(Some("s1"), "Page.navigate", json!({"url": "about:blank"}))
            .await
            .unwrap();
        assert_eq!(result["url"], "about:blank");
        let event = events.recv().await.unwrap();
        assert_eq!(event.method, "Page.loadEventFired");
        assert_eq!(event.session.as_deref(), Some("s1"));

        let error = cdp.call(None, "Bad.call", json!({})).await.unwrap_err();
        assert!(error.to_string().contains("no such method"));
    }
}
