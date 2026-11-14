//! Plots. With the UI comm open, ark announces each plot as a `positron.plot`
//! comm carrying a pre-rendered PNG, and re-renders on request at any size and
//! format. Hosted by an IDE, the console relays these to the IDE's plot pane;
//! standalone, it draws them inline in terminals that support images.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::ide::Ide;
use crate::jupyter::{Channel, KernelClient, Message};

pub const PLOT_COMM_TARGET: &str = "positron.plot";

/// ark sends plot updates as unpadded base64; strict decoders (terminals,
/// `atob`) want the padding.
fn pad_base64(data: &str) -> String {
    let mut out: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    while !out.len().is_multiple_of(4) {
        out.push('=');
    }
    out
}

/// A plot result (`{data, mime_type, settings}`) with normalized base64.
fn normalized(result: &Value) -> Value {
    let mut result = result.clone();
    if let Some(data) = result["data"].as_str() {
        result["data"] = Value::String(pad_base64(data));
    }
    result
}

pub enum PlotEvent {
    /// Not a plot message.
    None,
    /// A plot message, already dealt with.
    Handled,
    /// A plot to draw inline (base64 PNG).
    Image(String),
}

pub struct PlotRelay {
    ide: Option<Arc<Ide>>,
    ui_comm: Mutex<Option<String>>,
    comms: Mutex<HashSet<String>>,
    /// Jupyter `msg_id` of a render request -> the IDE's request id.
    pending: Mutex<HashMap<String, Value>>,
}

impl PlotRelay {
    pub fn new(ide: Option<Arc<Ide>>) -> Self {
        Self {
            ide,
            ui_comm: Mutex::new(None),
            comms: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn hosted(&self) -> bool {
        self.ide.is_some()
    }

    pub fn set_ui_comm(&self, comm_id: &str) {
        *self.ui_comm.lock().unwrap() = Some(comm_id.to_string());
    }

    pub fn handle(&self, msg: &Message) -> PlotEvent {
        let c = &msg.content;
        let comm_id = c["comm_id"].as_str().unwrap_or_default();
        match msg.msg_type() {
            "comm_open" if c["target_name"] == PLOT_COMM_TARGET => {
                self.comms.lock().unwrap().insert(comm_id.to_string());
                self.announce("plot_open", comm_id, &c["data"]["pre_render"])
            },
            "comm_msg" if self.comms.lock().unwrap().contains(comm_id) => {
                let data = &c["data"];
                let request = msg.parent_id().and_then(|p| self.pending.lock().unwrap().remove(p));
                if let (Some(id), Some(ide)) = (request, &self.ide) {
                    ide.send(json!({
                        "type": "plot_render_reply",
                        "id": id,
                        "result": normalized(&data["result"]),
                        "error": data["error"],
                    }));
                    return PlotEvent::Handled;
                }
                match data["method"].as_str() {
                    Some("update" | "show") => self.announce("plot_update", comm_id, &data["params"]["pre_render"]),
                    _ => PlotEvent::Handled,
                }
            },
            "comm_close" if self.comms.lock().unwrap().remove(comm_id) => {
                if let Some(ide) = &self.ide {
                    ide.send(json!({ "type": "plot_close", "comm_id": comm_id }));
                }
                PlotEvent::Handled
            },
            _ => PlotEvent::None,
        }
    }

    fn announce(&self, kind: &str, comm_id: &str, pre_render: &Value) -> PlotEvent {
        let pre_render = &normalized(pre_render);
        match &self.ide {
            Some(ide) => {
                ide.send(json!({ "type": kind, "comm_id": comm_id, "pre_render": pre_render }));
                PlotEvent::Handled
            },
            None => match pre_render["data"].as_str() {
                Some(data) if pre_render["mime_type"] == "image/png" => PlotEvent::Image(data.to_string()),
                _ => PlotEvent::Handled,
            },
        }
    }

    /// Sends the IDE's queued plot requests (render, size hints, close) to the kernel.
    pub fn send_requests(&self, client: &KernelClient) {
        let Some(ide) = &self.ide else { return };
        for req in ide.take_plot_requests() {
            let comm_id = req["comm_id"].as_str().unwrap_or_default();
            match req["type"].as_str() {
                Some("plot_render") if !self.comms.lock().unwrap().contains(comm_id) => {
                    ide.send(json!({
                        "type": "plot_render_reply",
                        "id": req["id"],
                        "error": { "message": format!("no such plot: {comm_id}") },
                    }));
                },
                Some("plot_render") => {
                    let data = json!({
                        "jsonrpc": "2.0",
                        "id": req["id"].to_string(),
                        "method": "render",
                        "params": {
                            "size": req["size"],
                            "pixel_ratio": req["pixel_ratio"],
                            "format": req["format"],
                        },
                    });
                    match client.send(Channel::Shell, "comm_msg", json!({ "comm_id": comm_id, "data": data })) {
                        Ok(msg_id) => {
                            self.pending.lock().unwrap().insert(msg_id, req["id"].clone());
                        },
                        Err(err) => ide.send(json!({
                            "type": "plot_render_reply",
                            "id": req["id"],
                            "error": { "message": err.to_string() },
                        })),
                    }
                },
                // New plots are pre-rendered at this size, so they arrive sharp.
                Some("plot_settings") => {
                    if let Some(ui) = self.ui_comm.lock().unwrap().clone() {
                        let _ = client.send(
                            Channel::Shell,
                            "comm_msg",
                            json!({
                                "comm_id": ui,
                                "data": {
                                    "jsonrpc": "2.0",
                                    "method": "did_change_plots_render_settings",
                                    "params": { "settings": {
                                        "size": req["size"],
                                        "pixel_ratio": req["pixel_ratio"],
                                        "format": "png",
                                    } },
                                },
                            }),
                        );
                    }
                },
                // The IDE dropped the plot from its history: free ark's recording.
                Some("plot_close") => {
                    self.comms.lock().unwrap().remove(comm_id);
                    let _ = client.send(Channel::Shell, "comm_close", json!({ "comm_id": comm_id, "data": {} }));
                },
                _ => {},
            }
        }
    }
}

/// Terminal inline-image protocols.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageProtocol {
    /// iTerm2's OSC 1337 (also WezTerm, VS Code with `terminal.integrated.enableImages`).
    Iterm,
    Kitty,
}

/// `BETTERR_INLINE_IMAGES=iterm|kitty|none` overrides detection.
pub fn detect_image_protocol() -> Option<ImageProtocol> {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    match var("BETTERR_INLINE_IMAGES").as_str() {
        "iterm" => return Some(ImageProtocol::Iterm),
        "kitty" => return Some(ImageProtocol::Kitty),
        "none" => return None,
        _ => {},
    }
    if var("TERM") == "xterm-kitty" || !var("KITTY_WINDOW_ID").is_empty() {
        return Some(ImageProtocol::Kitty);
    }
    match var("TERM_PROGRAM").as_str() {
        "iTerm.app" | "WezTerm" => Some(ImageProtocol::Iterm),
        _ if var("LC_TERMINAL") == "iTerm2" => Some(ImageProtocol::Iterm),
        _ => None,
    }
}

/// Escape sequence that draws a base64 PNG at the cursor.
pub fn inline_image(protocol: ImageProtocol, png_base64: &str) -> String {
    match protocol {
        ImageProtocol::Iterm => format!("\x1b]1337;File=inline=1;preserveAspectRatio=1:{png_base64}\x07"),
        ImageProtocol::Kitty => {
            // Kitty takes the payload in chunks of at most 4096 bytes.
            let chunks: Vec<&[u8]> = png_base64.as_bytes().chunks(4096).collect();
            let mut out = String::new();
            for (i, chunk) in chunks.iter().enumerate() {
                let more = u8::from(i + 1 < chunks.len());
                let chunk = std::str::from_utf8(chunk).unwrap_or_default();
                if i == 0 {
                    out.push_str(&format!("\x1b_Ga=T,f=100,m={more};{chunk}\x1b\\"));
                } else {
                    out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
                }
            }
            out
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_padded() {
        assert_eq!(pad_base64("QUJDRA"), "QUJDRA==");
        assert_eq!(pad_base64("QUJD\nRA=="), "QUJDRA==");
        assert_eq!(normalized(&json!({ "data": "QQ", "mime_type": "image/png" }))["data"], "QQ==");
    }

    #[test]
    fn kitty_payload_is_chunked() {
        let data = "A".repeat(5000);
        let seq = inline_image(ImageProtocol::Kitty, &data);
        assert!(seq.starts_with("\x1b_Ga=T,f=100,m=1;"));
        assert!(seq.contains("\x1b_Gm=0;"));
        assert_eq!(seq.matches('A').count(), 5000);
    }

    #[test]
    fn iterm_sequence_wraps_payload() {
        assert_eq!(
            inline_image(ImageProtocol::Iterm, "QUJD"),
            "\x1b]1337;File=inline=1;preserveAspectRatio=1:QUJD\x07"
        );
    }
}
