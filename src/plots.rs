//! Plots. With the UI comm open, ark announces each plot as a `positron.plot`
//! comm carrying a pre-rendered PNG, and re-renders on request at any size and
//! format. Hosted by an IDE, the console relays these to the IDE's plot pane;
//! standalone, it draws them inline in terminals that support images.
//!
//! The same relay carries the IDE's other comm traffic: help, interactive
//! output, the variables comm (environment explorer) and generic comm RPCs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};

use crate::config::InlinePlots;
use crate::ide::Ide;
use crate::jupyter::{Channel, KernelClient, Message};

pub const PLOT_COMM_TARGET: &str = "positron.plot";
pub const HELP_COMM_TARGET: &str = "positron.help";
pub const VARIABLES_COMM_TARGET: &str = "positron.variables";
/// Opened by ark for `View(df)` or a variables `view` request.
pub const DATA_EXPLORER_COMM_TARGET: &str = "positron.dataExplorer";

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
    /// Interactive output (an htmlwidget file, a local web app URL, a file to
    /// open) with no IDE to show it: hand it to the system browser/opener.
    Open { target: String, title: String },
    /// Something only the IDE can show (e.g. `View(df)`): say so.
    Note(&'static str),
}

pub struct PlotRelay {
    ide: Option<Arc<Ide>>,
    ui_comm: Mutex<Option<String>>,
    help_comm: Mutex<Option<String>>,
    variables_comm: Mutex<Option<String>>,
    /// Jupyter `msg_id` of a help-topic lookup -> the IDE's request id.
    pending_help: Mutex<HashMap<String, Value>>,
    comms: Mutex<HashSet<String>>,
    /// Jupyter `msg_id` of a render request -> the IDE's request id.
    pending: Mutex<HashMap<String, Value>>,
    /// Jupyter `msg_id` of a generic comm RPC (`comm_rpc`) -> the IDE's request id.
    pending_rpc: Mutex<HashMap<String, Value>>,
    /// Data explorer comms ark opened, which the IDE shows and addresses by id.
    data_comms: Mutex<HashSet<String>>,
}

impl PlotRelay {
    pub fn new(ide: Option<Arc<Ide>>) -> Self {
        Self {
            ide,
            ui_comm: Mutex::new(None),
            help_comm: Mutex::new(None),
            variables_comm: Mutex::new(None),
            pending_help: Mutex::new(HashMap::new()),
            comms: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            pending_rpc: Mutex::new(HashMap::new()),
            data_comms: Mutex::new(HashSet::new()),
        }
    }

    pub fn hosted(&self) -> bool {
        self.ide.is_some()
    }

    pub fn set_ui_comm(&self, comm_id: &str) {
        *self.ui_comm.lock().unwrap() = Some(comm_id.to_string());
    }

    pub fn set_help_comm(&self, comm_id: &str) {
        *self.help_comm.lock().unwrap() = Some(comm_id.to_string());
    }

    pub fn set_variables_comm(&self, comm_id: &str) {
        *self.variables_comm.lock().unwrap() = Some(comm_id.to_string());
    }

    pub fn handle(&self, msg: &Message) -> PlotEvent {
        let c = &msg.content;
        let comm_id = c["comm_id"].as_str().unwrap_or_default();
        if msg.msg_type() == "comm_msg"
            && let Some(id) = msg.parent_id().and_then(|p| self.pending_rpc.lock().unwrap().remove(p))
        {
            if let Some(ide) = &self.ide {
                ide.send(json!({
                    "type": "comm_rpc_reply",
                    "id": id,
                    "result": c["data"]["result"],
                    "error": c["data"]["error"],
                }));
            }
            return PlotEvent::Handled;
        }
        match msg.msg_type() {
            // Environment explorer: `refresh` (full list) and `update` (diff)
            // after each execution, for the global env or the debug frame.
            "comm_msg" if self.variables_comm.lock().unwrap().as_deref() == Some(comm_id) => {
                if let Some(ide) = &self.ide {
                    ide.send(json!({
                        "type": "variables_event",
                        "method": c["data"]["method"],
                        "params": c["data"]["params"],
                    }));
                }
                PlotEvent::Handled
            },
            "comm_msg" if self.ui_comm.lock().unwrap().as_deref() == Some(comm_id) => self.handle_ui(&c["data"]),
            "comm_msg" if self.help_comm.lock().unwrap().as_deref() == Some(comm_id) => self.handle_help(msg),
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
            // Data explorer (`View(df)`): the IDE shows the table and queries
            // it with `comm_rpc`; ark sends `schema_update` / `data_update`
            // when the variable changes and closes the comm when it's gone.
            "comm_open" if c["target_name"] == DATA_EXPLORER_COMM_TARGET => {
                let Some(ide) = &self.ide else {
                    return PlotEvent::Note("[View(): the data viewer only works in VS Code, with the betteR extension]");
                };
                self.data_comms.lock().unwrap().insert(comm_id.to_string());
                ide.send(json!({
                    "type": "comm_open",
                    "comm_id": comm_id,
                    "target": DATA_EXPLORER_COMM_TARGET,
                    "data": c["data"],
                }));
                PlotEvent::Handled
            },
            "comm_msg" if self.data_comms.lock().unwrap().contains(comm_id) => {
                if let Some(ide) = &self.ide {
                    ide.send(json!({
                        "type": "comm_event",
                        "comm_id": comm_id,
                        "method": c["data"]["method"],
                        "params": c["data"]["params"],
                    }));
                }
                PlotEvent::Handled
            },
            "comm_close" if self.data_comms.lock().unwrap().remove(comm_id) => {
                if let Some(ide) = &self.ide {
                    ide.send(json!({ "type": "comm_closed", "comm_id": comm_id }));
                }
                PlotEvent::Handled
            },
            _ => PlotEvent::None,
        }
    }

    /// Interactive output from ark's UI comm: htmlwidgets (`show_html_file`),
    /// web apps such as Shiny (`show_url`) and other files (`open_with_system`).
    /// Other UI events (prompt, width, ...) are left to the REPL.
    fn handle_ui(&self, data: &Value) -> PlotEvent {
        let params = &data["params"];
        // Sent after every execution: the IDE refreshes session state on it.
        // Left to the REPL too, which tracks the prompt.
        if data["method"] == "prompt_state" {
            if let Some(ide) = &self.ide {
                ide.send(json!({ "type": "prompt_state", "params": params }));
            }
            return PlotEvent::None;
        }
        let (target, title) = match data["method"].as_str() {
            Some("show_html_file") => (&params["path"], params["title"].as_str().unwrap_or("R")),
            Some("show_url") => (&params["url"], "R"),
            Some("open_with_system") => (&params["path"], "R"),
            _ => return PlotEvent::None,
        };
        let Some(target) = target.as_str() else {
            return PlotEvent::Handled;
        };
        match &self.ide {
            Some(ide) => {
                ide.send(json!({ "type": data["method"], "params": params }));
                PlotEvent::Handled
            },
            None => PlotEvent::Open {
                target: target.to_string(),
                title: title.to_string(),
            },
        }
    }

    /// Help pages (`?topic`, `help()`, `??search`): `show_help` events carry a
    /// URL on ark's local help proxy. Also replies to IDE topic lookups.
    fn handle_help(&self, msg: &Message) -> PlotEvent {
        let data = &msg.content["data"];
        let request = msg.parent_id().and_then(|p| self.pending_help.lock().unwrap().remove(p));
        if let (Some(id), Some(ide)) = (request, &self.ide) {
            let found = data["result"].as_bool().unwrap_or(false);
            ide.send(json!({ "type": "help_topic_reply", "id": id, "found": found }));
            return PlotEvent::Handled;
        }
        if data["method"] != "show_help" {
            return PlotEvent::Handled;
        }
        let params = &data["params"];
        match (&self.ide, params["content"].as_str()) {
            (Some(ide), _) => {
                ide.send(json!({ "type": "show_help", "params": params }));
                PlotEvent::Handled
            },
            (None, Some(url)) if params["kind"] == "url" => PlotEvent::Open {
                target: url.to_string(),
                title: "R help".into(),
            },
            _ => PlotEvent::Handled,
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
                // Help for a topic (e.g. F1 in the editor); ark replies with
                // whether it was found and, if so, sends `show_help`.
                Some("help_topic") => {
                    let Some(help) = self.help_comm.lock().unwrap().clone() else {
                        ide.send(json!({ "type": "help_topic_reply", "id": req["id"], "found": false }));
                        continue;
                    };
                    let data = json!({
                        "jsonrpc": "2.0",
                        "id": req["id"].to_string(),
                        "method": "show_help_topic",
                        "params": { "topic": req["topic"] },
                    });
                    if let Ok(msg_id) = client.send(Channel::Shell, "comm_msg", json!({ "comm_id": help, "data": data })) {
                        self.pending_help.lock().unwrap().insert(msg_id, req["id"].clone());
                    }
                },
                // A JSON-RPC request on the variables comm (`inspect`, `delete`,
                // ...) or the UI comm (`evaluate_code`); answered with `comm_rpc_reply`.
                Some("comm_rpc") => {
                    let comm = match (req["comm"].as_str(), req["comm_id"].as_str()) {
                        (Some("variables"), _) => self.variables_comm.lock().unwrap().clone(),
                        (Some("ui"), _) => self.ui_comm.lock().unwrap().clone(),
                        (_, Some(id)) if self.data_comms.lock().unwrap().contains(id) => Some(id.to_string()),
                        _ => None,
                    };
                    let Some(comm) = comm else {
                        ide.send(json!({
                            "type": "comm_rpc_reply",
                            "id": req["id"],
                            "error": { "message": format!("comm not available: {}", if req["comm"].is_null() { &req["comm_id"] } else { &req["comm"] }) },
                        }));
                        continue;
                    };
                    let mut data = json!({ "jsonrpc": "2.0", "id": req["id"].to_string(), "method": req["method"] });
                    if !req["params"].is_null() {
                        data["params"] = req["params"].clone();
                    }
                    match client.send(Channel::Shell, "comm_msg", json!({ "comm_id": comm, "data": data })) {
                        Ok(msg_id) => {
                            self.pending_rpc.lock().unwrap().insert(msg_id, req["id"].clone());
                        },
                        Err(err) => ide.send(json!({
                            "type": "comm_rpc_reply",
                            "id": req["id"],
                            "error": { "message": err.to_string() },
                        })),
                    }
                },
                // The IDE closed a data explorer: free ark's view of the table.
                Some("comm_close") => {
                    if self.data_comms.lock().unwrap().remove(comm_id) {
                        let _ = client.send(Channel::Shell, "comm_close", json!({ "comm_id": comm_id, "data": {} }));
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
    /// DEC Sixel (Windows Terminal 1.22+, also xterm, foot, VS Code with
    /// `terminal.integrated.enableImages`). Takes pixels, not a PNG.
    Sixel,
}

/// `startup.inline_plots` other than `auto` overrides detection.
pub fn detect_image_protocol(setting: InlinePlots) -> Option<ImageProtocol> {
    match setting {
        InlinePlots::Iterm => return Some(ImageProtocol::Iterm),
        InlinePlots::Kitty => return Some(ImageProtocol::Kitty),
        InlinePlots::Sixel => return Some(ImageProtocol::Sixel),
        InlinePlots::None => return None,
        InlinePlots::Auto => {},
    }
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    if var("TERM") == "xterm-kitty" || !var("KITTY_WINDOW_ID").is_empty() {
        return Some(ImageProtocol::Kitty);
    }
    match var("TERM_PROGRAM").as_str() {
        "iTerm.app" | "WezTerm" => Some(ImageProtocol::Iterm),
        _ if var("LC_TERMINAL") == "iTerm2" => Some(ImageProtocol::Iterm),
        // Windows Terminal. VS Code started from it inherits `WT_SESSION`, but
        // sets `TERM_PROGRAM`, so it's ruled out by the arm above.
        "" if !var("WT_SESSION").is_empty() => Some(ImageProtocol::Sixel),
        _ => None,
    }
}

/// Escape sequence that draws a base64 PNG at the cursor.
pub fn inline_image(protocol: ImageProtocol, png_base64: &str) -> Result<String> {
    Ok(match protocol {
        ImageProtocol::Iterm => format!("\x1b]1337;File=inline=1;preserveAspectRatio=1:{png_base64}\x07"),
        ImageProtocol::Sixel => sixel(png_base64)?,
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
    })
}

fn sixel(png_base64: &str) -> Result<String> {
    let png = base64::engine::general_purpose::STANDARD.decode(png_base64)?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    // Expand palette and low bit depths, so every image is 8-bit gray or RGB(A).
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or_else(|| anyhow!("PNG too large"))?];
    let info = reader.next_frame(&mut buf)?;
    let pixels = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => pixels.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err(anyhow!("unexpanded indexed PNG")),
    };
    // No dithering: plots are mostly flat colours and antialiased edges, which
    // fit in 256 colours, and dithering would speckle text and lines.
    let opts = icy_sixel::EncodeOptions { diffusion: 0.0, ..Default::default() };
    Ok(icy_sixel::sixel_encode(&rgba, info.width as usize, info.height as usize, &opts)?)
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
        let seq = inline_image(ImageProtocol::Kitty, &data).unwrap();
        assert!(seq.starts_with("\x1b_Ga=T,f=100,m=1;"));
        assert!(seq.contains("\x1b_Gm=0;"));
        assert_eq!(seq.matches('A').count(), 5000);
    }

    #[test]
    fn iterm_sequence_wraps_payload() {
        assert_eq!(
            inline_image(ImageProtocol::Iterm, "QUJD").unwrap(),
            "\x1b]1337;File=inline=1;preserveAspectRatio=1:QUJD\x07"
        );
    }

    #[test]
    fn sixel_round_trips_a_png() {
        // 3x2 RGB: red, green, blue / white, black, gray.
        let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 0, 0, 0, 128, 128, 128];
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, 3, 2);
        encoder.set_color(png::ColorType::Rgb);
        encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(&png);

        let seq = inline_image(ImageProtocol::Sixel, &data).unwrap();
        assert!(seq.starts_with("\x1bP"), "not a DCS sequence: {seq:?}");
        assert!(seq.ends_with("\x1b\\"));
        // Raster attributes give the size; the decoder pads to a 6-pixel band.
        assert!(seq.contains("\"1;1;3;2"), "no raster attributes: {seq:?}");
        let image = icy_sixel::SixelImage::decode(seq.as_bytes()).unwrap();
        let pixel = |i: usize| &image.pixels[i * 4..i * 4 + 4];
        assert_eq!(pixel(0), &[255, 0, 0, 255]);
        assert_eq!(pixel(4), &[0, 0, 0, 255]);
        // Sixel colours are percentages, so 128 comes back as 127 or 128.
        assert!(pixel(5)[..3].iter().all(|c| c.abs_diff(128) <= 1), "{:?}", pixel(5));
    }

    #[test]
    fn sixel_rejects_garbage() {
        assert!(inline_image(ImageProtocol::Sixel, "QUJD").is_err());
    }
}
