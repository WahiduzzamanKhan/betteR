//! Minimal Jupyter messaging client (wire protocol v5.3) over ZeroMQ.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;

const DELIMITER: &[u8] = b"<IDS|MSG>";
const PROTOCOL_VERSION: &str = "5.3";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub shell_port: u16,
    pub iopub_port: u16,
    pub stdin_port: u16,
    pub control_port: u16,
    pub hb_port: u16,
    pub ip: String,
    pub key: String,
    pub transport: String,
    pub signature_scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_name: Option<String>,
}

impl ConnectionInfo {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading connection file {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn endpoint(&self, port: u16) -> String {
        format!("{}://{}:{}", self.transport, self.ip, port)
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub header: Value,
    pub parent_header: Value,
    pub content: Value,
}

impl Message {
    pub fn msg_type(&self) -> &str {
        self.header["msg_type"].as_str().unwrap_or("")
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent_header["msg_id"].as_str()
    }
}

fn sign(key: Option<&[u8]>, parts: &[&[u8]]) -> String {
    let Some(key) = key else {
        return String::new();
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    hex::encode(mac.finalize().into_bytes())
}

/// Parses (and, with a key, verifies) a multipart Jupyter message.
fn decode(frames: &[Vec<u8>], key: Option<&[u8]>) -> Result<Message> {
    let Some(delim) = frames.iter().position(|f| f == DELIMITER) else {
        bail!("malformed Jupyter message: no delimiter");
    };
    if frames.len() < delim + 6 {
        bail!("malformed Jupyter message: too few frames");
    }
    let [signature, header, parent, metadata, content] = [1, 2, 3, 4, 5].map(|i| &frames[delim + i]);
    if key.is_some() && sign(key, &[header, parent, metadata, content]).as_bytes() != signature.as_slice() {
        bail!("invalid message signature");
    }
    Ok(Message {
        header: serde_json::from_slice(header)?,
        parent_header: serde_json::from_slice(parent)?,
        content: serde_json::from_slice(content)?,
    })
}

/// A listen-only IOPub subscriber.
pub struct IopubListener {
    _ctx: zmq::Context,
    socket: zmq::Socket,
    key: Option<Vec<u8>>,
}

impl IopubListener {
    /// Subscribes to everything. Connecting before the kernel binds is fine:
    /// ZeroMQ connects (and subscribes) as soon as the port opens.
    pub fn connect(info: &ConnectionInfo) -> Result<Self> {
        let ctx = zmq::Context::new();
        let socket = ctx.socket(zmq::SUB)?;
        socket.set_linger(0)?;
        socket.set_subscribe(b"")?;
        socket.connect(&info.endpoint(info.iopub_port))?;
        Ok(Self {
            _ctx: ctx,
            socket,
            key: (!info.key.is_empty()).then(|| info.key.as_bytes().to_vec()),
        })
    }

    pub fn recv(&self) -> Result<Message> {
        decode(&self.socket.recv_multipart(0)?, self.key.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Shell,
    Control,
    Stdin,
    Iopub,
}

pub struct KernelClient {
    _ctx: zmq::Context,
    shell: zmq::Socket,
    control: zmq::Socket,
    stdin: zmq::Socket,
    iopub: zmq::Socket,
    key: Option<Vec<u8>>,
    session: String,
    alive: Arc<AtomicBool>,
}

impl KernelClient {
    pub fn connect(info: &ConnectionInfo) -> Result<Self> {
        if !info.key.is_empty() && info.signature_scheme != "hmac-sha256" {
            bail!("unsupported signature scheme {}", info.signature_scheme);
        }
        let ctx = zmq::Context::new();
        let session = uuid::Uuid::new_v4().to_string();

        // The kernel routes `input_request`s on stdin to the identity that sent
        // the `execute_request` on shell, so both sockets share one identity.
        let identity = session.as_bytes();
        let dealer = |port: u16| -> Result<zmq::Socket> {
            let s = ctx.socket(zmq::DEALER)?;
            s.set_identity(identity)?;
            s.set_linger(0)?;
            s.connect(&info.endpoint(port))?;
            Ok(s)
        };
        let shell = dealer(info.shell_port)?;
        let control = dealer(info.control_port)?;
        let stdin = dealer(info.stdin_port)?;
        let iopub = ctx.socket(zmq::SUB)?;
        iopub.set_linger(0)?;
        iopub.set_subscribe(b"")?;
        iopub.connect(&info.endpoint(info.iopub_port))?;

        let alive = Arc::new(AtomicBool::new(true));
        spawn_heartbeat(ctx.clone(), info.endpoint(info.hb_port), alive.clone());

        Ok(Self {
            _ctx: ctx,
            shell,
            control,
            stdin,
            iopub,
            key: (!info.key.is_empty()).then(|| info.key.as_bytes().to_vec()),
            session,
            alive,
        })
    }

    /// False once the kernel has stopped answering heartbeats.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn socket(&self, channel: Channel) -> &zmq::Socket {
        match channel {
            Channel::Shell => &self.shell,
            Channel::Control => &self.control,
            Channel::Stdin => &self.stdin,
            Channel::Iopub => &self.iopub,
        }
    }

    fn sign(&self, parts: &[&[u8]]) -> String {
        sign(self.key.as_deref(), parts)
    }

    /// Sends a message and returns its `msg_id`.
    pub fn send(&self, channel: Channel, msg_type: &str, content: Value) -> Result<String> {
        self.send_with_parent(channel, msg_type, content, json!({}))
    }

    pub fn send_with_parent(
        &self,
        channel: Channel,
        msg_type: &str,
        content: Value,
        parent_header: Value,
    ) -> Result<String> {
        let msg_id = uuid::Uuid::new_v4().to_string();
        let header = json!({
            "msg_id": msg_id,
            "session": self.session,
            // `USER` on Unix, `USERNAME` on Windows.
            "username": std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "user".into()),
            "date": chrono::Utc::now().to_rfc3339(),
            "msg_type": msg_type,
            "version": PROTOCOL_VERSION,
        });
        let header = serde_json::to_vec(&header)?;
        let parent = serde_json::to_vec(&parent_header)?;
        let metadata = b"{}".to_vec();
        let content = serde_json::to_vec(&content)?;
        let signature = self.sign(&[&header, &parent, &metadata, &content]);
        let frames: Vec<&[u8]> = vec![
            DELIMITER,
            signature.as_bytes(),
            &header,
            &parent,
            &metadata,
            &content,
        ];
        self.socket(channel).send_multipart(frames, 0)?;
        Ok(msg_id)
    }

    fn recv(&self, channel: Channel) -> Result<Message> {
        decode(&self.socket(channel).recv_multipart(0)?, self.key.as_deref())
    }

    /// Waits up to `timeout` for a message on any of `channels`.
    pub fn poll(&self, channels: &[Channel], timeout: Duration) -> Result<Option<(Channel, Message)>> {
        let mut items: Vec<zmq::PollItem> = channels
            .iter()
            .map(|c| self.socket(*c).as_poll_item(zmq::POLLIN))
            .collect();
        let n = match zmq::poll(&mut items, timeout.as_millis() as i64) {
            Ok(n) => n,
            // Ctrl-C (forwarded to the kernel as an interrupt) lands here.
            Err(zmq::Error::EINTR) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if n == 0 {
            return Ok(None);
        }
        let ready = items.iter().position(|i| i.is_readable());
        drop(items);
        match ready {
            Some(i) => Ok(Some((channels[i], self.recv(channels[i])?))),
            None => Ok(None),
        }
    }

    /// Sends a request and waits for the reply with matching parent on the same
    /// channel. Messages on other channels are left queued.
    pub fn request(&self, channel: Channel, msg_type: &str, content: Value, timeout: Duration) -> Result<Message> {
        let msg_id = self.send(channel, msg_type, content)?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for {msg_type} reply");
            }
            if let Some((_, msg)) = self.poll(&[channel], remaining)?
                && msg.parent_id() == Some(msg_id.as_str())
            {
                return Ok(msg);
            }
        }
    }
}

impl KernelClient {
    /// Sends `kernel_info_request` until both the shell reply and the matching
    /// IOPub status arrive, which guarantees the IOPub subscription is live.
    /// `abort` is polled between attempts and fails startup with its message.
    /// Asks `kernel_info` until the kernel answers, and returns the reply.
    /// Unrelated IOPub messages that arrive meanwhile, such as `.Rprofile`
    /// output (which has no parent request), go to `on_other`.
    pub fn wait_ready(
        &self,
        timeout: Duration,
        mut abort: impl FnMut() -> Option<String>,
        mut on_other: impl FnMut(Message),
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() > deadline {
                bail!("kernel did not become ready within {timeout:?}");
            }
            if let Some(reason) = abort() {
                bail!(reason);
            }
            let id = self.send(Channel::Shell, "kernel_info_request", json!({}))?;
            let (mut reply, mut iopub) = (None, false);
            let until = Instant::now() + Duration::from_millis(500);
            while Instant::now() < until {
                let Some((channel, msg)) = self.poll(&[Channel::Shell, Channel::Iopub], Duration::from_millis(50))?
                else {
                    continue;
                };
                if msg.parent_id() != Some(id.as_str()) {
                    if channel == Channel::Iopub {
                        on_other(msg);
                    }
                    continue;
                }
                match channel {
                    Channel::Shell => reply = Some(msg.content),
                    _ => iopub = true,
                }
                if iopub && let Some(reply) = reply.take() {
                    return Ok(reply);
                }
            }
        }
    }

    pub fn open_comm(&self, target: &str, data: Value) -> Result<String> {
        let comm_id = uuid::Uuid::new_v4().to_string();
        self.send(
            Channel::Shell,
            "comm_open",
            json!({ "comm_id": comm_id, "target_name": target, "data": data }),
        )?;
        Ok(comm_id)
    }

    /// Opens a server comm (`lsp`, `ark_dap`) and returns its comm id and the
    /// TCP port ark reports in `server_started`. Other IOPub traffic goes to `on_other`.
    pub fn start_server_comm(
        &self,
        target: &str,
        timeout: Duration,
        mut on_other: impl FnMut(&Message),
    ) -> Result<(String, u16)> {
        let comm_id = self.open_comm(target, json!({ "ip_address": "127.0.0.1" }))?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Some((_, msg)) = self.poll(&[Channel::Iopub], Duration::from_millis(100))? else {
                continue;
            };
            let data = &msg.content["data"];
            if msg.msg_type() == "comm_msg" && msg.content["comm_id"] == comm_id.as_str() {
                if data["method"] == "server_started"
                    && let Some(port) = data["params"]["port"].as_u64()
                {
                    return Ok((comm_id, port as u16));
                }
            } else {
                on_other(&msg);
            }
        }
        bail!("ark did not start its {target} server")
    }
}

/// Pings the kernel's heartbeat socket every second and clears `alive` after
/// several missed pongs.
fn spawn_heartbeat(ctx: zmq::Context, endpoint: String, alive: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let make = || -> Option<zmq::Socket> {
            let s = ctx.socket(zmq::REQ).ok()?;
            s.set_linger(0).ok()?;
            s.connect(&endpoint).ok()?;
            Some(s)
        };
        let mut misses = 0;
        let mut sock = make();
        loop {
            let Some(s) = sock.as_ref() else { return };
            let ok = s.send("ping", 0).is_ok() && matches!(s.poll(zmq::POLLIN, 3000), Ok(n) if n > 0);
            if ok && s.recv_bytes(0).is_ok() {
                misses = 0;
                alive.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(1));
            } else {
                misses += 1;
                if misses >= 3 {
                    alive.store(false, Ordering::Relaxed);
                }
                // A REQ socket that missed its reply is stuck; replace it.
                sock = make();
            }
        }
    });
}
