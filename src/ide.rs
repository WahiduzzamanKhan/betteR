//! Channel between the console and its host IDE (the VS Code extension),
//! carried as JSON lines over a Unix socket the IDE listens on.
//!
//! Console -> IDE: `dap_port`, `debug` (start/stop), `complete` requests,
//! plot events (`plot_open`, `plot_update`, `plot_close`, `plot_render_reply`),
//! interactive output (`show_html_file`, `show_url`, `open_with_system`),
//! help (`show_help`, `help_topic_reply`).
//! IDE -> console: `execute` (code + optional file location), `complete_reply`,
//! plot requests (`plot_render`, `plot_settings`, `plot_close`), `help_topic`.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub struct Ide {
    writer: Mutex<UnixStream>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    inbox: Mutex<VecDeque<Value>>,
    /// Plot requests are served by the background IOPub thread without
    /// interrupting line editing, so they're kept apart from `inbox`.
    plot_requests: Mutex<VecDeque<Value>>,
    next_id: AtomicU64,
}

impl Ide {
    /// Connects and starts reading. `wake` is raised whenever an event that
    /// needs the main loop (e.g. `execute`) arrives, so line editing can yield.
    pub fn connect(path: &Path, wake: Arc<AtomicBool>) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(path).with_context(|| format!("connecting to IDE at {}", path.display()))?;
        let reader = BufReader::new(stream.try_clone()?);
        let ide = Arc::new(Self {
            writer: Mutex::new(stream),
            pending: Mutex::new(HashMap::new()),
            inbox: Mutex::new(VecDeque::new()),
            plot_requests: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(1),
        });
        let weak = Arc::downgrade(&ide);
        std::thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(ide) = weak.upgrade() else { break };
                if msg["type"] == "complete_reply" {
                    if let Some(tx) = msg["id"].as_u64().and_then(|id| ide.pending.lock().unwrap().remove(&id)) {
                        let _ = tx.send(msg["items"].clone());
                    }
                } else if msg["type"].as_str().is_some_and(|t| t.starts_with("plot_") || t.starts_with("help_")) {
                    ide.plot_requests.lock().unwrap().push_back(msg);
                } else {
                    ide.inbox.lock().unwrap().push_back(msg);
                    wake.store(true, Ordering::SeqCst);
                }
            }
        });
        Ok(ide)
    }

    pub fn send(&self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        let _ = self.writer.lock().unwrap().write_all(line.as_bytes());
    }

    pub fn take_events(&self) -> Vec<Value> {
        self.inbox.lock().unwrap().drain(..).collect()
    }

    pub fn take_plot_requests(&self) -> Vec<Value> {
        self.plot_requests.lock().unwrap().drain(..).collect()
    }

    /// Asks the IDE's language client for completions. `offset` is a byte
    /// offset into `text`; returned items use UTF-16 positions.
    pub fn complete(&self, text: &str, offset: usize) -> Result<Vec<Value>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "type": "complete", "id": id, "text": text, "offset": offset }));
        let items = rx.recv_timeout(Duration::from_secs(3));
        self.pending.lock().unwrap().remove(&id);
        Ok(match items? {
            Value::Array(items) => items,
            Value::Object(mut list) => match list.remove("items") {
                Some(Value::Array(items)) => items,
                _ => vec![],
            },
            _ => vec![],
        })
    }
}
