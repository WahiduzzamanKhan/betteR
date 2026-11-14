//! The interactive loop: reedline input, Jupyter execution, output rendering.

use std::borrow::Cow;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use nu_ansi_term::{Color, Style};
use reedline::{
    default_emacs_keybindings, ColumnarMenu, DefaultHinter, EditCommand, Emacs, FileBackedHistory, KeyCode,
    KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, ValidationResult, Validator,
};
use serde_json::{json, Value};

use crate::highlight::RHighlighter;
use crate::jupyter::{Channel, KernelClient, Message};
use crate::kernel::SpawnedKernel;
use crate::lsp::{LspClient, LspCompleter};

const UI_COMM_TARGET: &str = "positron.ui";

#[derive(Clone)]
struct RPrompt {
    input: String,
    continuation: String,
}

impl RPrompt {
    fn is_browser(&self) -> bool {
        is_browser_prompt(&self.input)
    }
}

fn is_browser_prompt(prompt: &str) -> bool {
    prompt
        .strip_prefix("Browse[")
        .and_then(|rest| rest.split_once("]>"))
        .is_some_and(|(depth, _)| !depth.is_empty() && depth.chars().all(|c| c.is_ascii_digit()))
}

impl Prompt for RPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.input)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.continuation)
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        let failing = match search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!("({failing}reverse-i-search: {}) ", search.term))
    }
    fn get_prompt_color(&self) -> reedline::Color {
        if self.is_browser() { reedline::Color::Yellow } else { reedline::Color::Blue }
    }
    fn get_prompt_multiline_color(&self) -> nu_ansi_term::Color {
        Color::DarkGray
    }
}

/// Plain prompt for `readline()`-style input requests.
struct InputPrompt(String);

impl Prompt for InputPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn get_prompt_color(&self) -> reedline::Color {
        reedline::Color::Default
    }
}

/// Enter submits only when the kernel says the input parses as complete;
/// otherwise reedline inserts a newline and shows the continuation prompt.
struct KernelValidator {
    client: Arc<Mutex<KernelClient>>,
}

impl Validator for KernelValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let client = self.client.lock().unwrap();
        let reply = client.request(
            Channel::Shell,
            "is_complete_request",
            json!({ "code": line }),
            Duration::from_secs(2),
        );
        match reply {
            Ok(m) if m.content["status"] == "incomplete" => ValidationResult::Incomplete,
            _ => ValidationResult::Complete,
        }
    }
}

pub enum Origin {
    Spawned(SpawnedKernel),
    Attached,
}

pub struct Repl {
    client: Arc<Mutex<KernelClient>>,
    editor: Option<Reedline>,
    input_editor: Option<Reedline>,
    prompt: RPrompt,
    origin: Origin,
    ui_comm: Option<String>,
    interrupted: Arc<AtomicBool>,
    /// Text written since the last newline, so input prompts can continue it.
    partial_line: String,
    width: u16,
}

enum Outcome {
    Done { error: bool, interrupted: bool },
    KernelGone,
}

/// Splits input into top-level R expressions. ark keeps evaluating the
/// remaining expressions of a request after an interrupt, so the console sends
/// them one at a time and stops on interrupt or error, like R's own console.
/// Input that doesn't parse is sent whole so R reports the syntax error.
fn split_top_level(code: &str) -> Vec<String> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&tree_sitter_r::LANGUAGE.into()).is_err() {
        return vec![code.to_string()];
    }
    let Some(tree) = parser.parse(code, None) else {
        return vec![code.to_string()];
    };
    let root = tree.root_node();
    if root.has_error() {
        return vec![code.to_string()];
    }
    let mut cursor = root.walk();
    let chunks: Vec<String> = root
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .map(|n| code[n.byte_range()].to_string())
        .collect();
    if chunks.is_empty() { vec![code.to_string()] } else { chunks }
}

impl Repl {
    pub fn start(client: KernelClient, origin: Origin, use_lsp: bool, interrupted: Arc<AtomicBool>) -> Result<Self> {
        let mut repl = Self {
            client: Arc::new(Mutex::new(client)),
            editor: None,
            input_editor: None,
            prompt: RPrompt {
                input: "> ".into(),
                continuation: "+ ".into(),
            },
            origin,
            ui_comm: None,
            interrupted,
            partial_line: String::new(),
            width: terminal_width(),
        };
        let info = repl.wait_ready(Duration::from_secs(60))?;
        repl.open_ui_comm()?;

        let interactive = std::io::stdin().is_terminal();
        let lsp = if use_lsp && interactive {
            match repl.open_lsp() {
                Ok(lsp) => Some(Arc::new(Mutex::new(lsp))),
                Err(err) => {
                    eprintln!("{}", Color::Yellow.paint(format!("completion unavailable: {err:#}")));
                    None
                },
            }
        } else {
            None
        };

        if interactive {
            let r_version = info["language_info"]["version"].as_str().unwrap_or("R");
            let ark = info["implementation_version"].as_str().unwrap_or("?");
            let how = match &repl.origin {
                Origin::Spawned(_) => "new session",
                Origin::Attached => "attached to running session",
            };
            let dim = Color::DarkGray.normal();
            println!("{}", Color::Blue.bold().paint(r_version.trim()));
            println!("{}", dim.paint(format!("ark {ark} · {how}")));
            println!("{}", dim.paint("Tab completes · Alt-Enter newline · Ctrl-C interrupts · Ctrl-D quits"));
            repl.editor = Some(repl.build_editor(lsp)?);
            repl.input_editor = Some(Reedline::create());
        }
        Ok(repl)
    }

    fn build_editor(&self, lsp: Option<Arc<Mutex<LspClient>>>) -> Result<Reedline> {
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion_menu".into()),
                ReedlineEvent::MenuNext,
            ]),
        );
        keybindings.add_binding(KeyModifiers::SHIFT, KeyCode::BackTab, ReedlineEvent::MenuPrevious);
        keybindings.add_binding(
            KeyModifiers::ALT,
            KeyCode::Enter,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );

        let mut editor = Reedline::create()
            .with_highlighter(Box::new(RHighlighter::new()))
            .with_validator(Box::new(KernelValidator {
                client: self.client.clone(),
            }))
            .with_completer(Box::new(LspCompleter { lsp }))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(
                ColumnarMenu::default().with_name("completion_menu").with_marker(""),
            )))
            .with_quick_completions(true)
            .with_partial_completions(true)
            .with_hinter(Box::new(DefaultHinter::default().with_style(Style::new().fg(Color::DarkGray))))
            .with_edit_mode(Box::new(Emacs::new(keybindings)))
            .use_bracketed_paste(true);
        if let Some(path) = history_path() {
            editor = editor.with_history(Box::new(FileBackedHistory::with_file(10_000, path)?));
        }
        Ok(editor)
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<Value> {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        client.wait_ready(timeout, || {
            self.kernel_exited().then(|| format!("kernel exited during startup{}", self.log_hint()))
        })
    }

    /// The UI comm gives us prompt changes (e.g. `Browse[1]>`) and lets R
    /// format output to the terminal width.
    fn open_ui_comm(&mut self) -> Result<()> {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        let comm_id = client.open_comm(UI_COMM_TARGET, json!({ "console_width": self.width }))?;
        self.ui_comm = Some(comm_id);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some((_, msg)) = client.poll(&[Channel::Iopub], Duration::from_millis(100))? {
                let is_prompt = msg.msg_type() == "comm_msg" && msg.content["data"]["method"] == "prompt_state";
                self.handle_iopub(&msg);
                if is_prompt {
                    break;
                }
            }
        }
        Ok(())
    }

    fn open_lsp(&mut self) -> Result<LspClient> {
        let client = self.client.clone();
        let port = client
            .lock()
            .unwrap()
            .start_server_comm("lsp", Duration::from_secs(15), |msg| self.handle_iopub(msg))?;
        LspClient::connect(port, &std::env::current_dir()?)
    }

    pub fn run(&mut self) -> Result<()> {
        loop {
            self.drain_iopub()?;
            self.sync_width();
            self.finish_line();
            if self.kernel_gone() {
                break;
            }
            let prompt = self.prompt.clone();
            let signal = self.editor.as_mut().expect("interactive editor").read_line(&prompt)?;
            match signal {
                Signal::Success(code) => {
                    // An empty line is meaningful in the debugger (it steps).
                    if code.trim().is_empty() && !prompt.is_browser() {
                        continue;
                    }
                    match self.execute_all(&code)? {
                        Outcome::KernelGone => break,
                        Outcome::Done { .. } => {},
                    }
                },
                Signal::CtrlD => break,
                _ => continue,
            }
        }
        self.finish_line();
        self.shutdown();
        Ok(())
    }

    /// Non-interactive mode: run piped input, stopping at the first error.
    /// Returns false on an R error or if the R session died.
    pub fn run_script(&mut self, code: &str) -> Result<bool> {
        let ok = match self.execute_all(code)? {
            Outcome::Done { error, .. } => !error,
            Outcome::KernelGone => true,
        };
        self.finish_line();
        let clean = self.shutdown();
        Ok(ok && clean)
    }

    fn execute_all(&mut self, code: &str) -> Result<Outcome> {
        let mut last = Outcome::Done { error: false, interrupted: false };
        for chunk in split_top_level(code) {
            last = self.execute(&chunk)?;
            if !matches!(last, Outcome::Done { error: false, interrupted: false }) {
                break;
            }
        }
        Ok(last)
    }

    fn execute(&mut self, code: &str) -> Result<Outcome> {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        let msg_id = client.send(
            Channel::Shell,
            "execute_request",
            json!({
                "code": code,
                "silent": false,
                "store_history": true,
                "user_expressions": {},
                "allow_stdin": true,
                "stop_on_error": true,
            }),
        )?;
        self.interrupted.store(false, Ordering::SeqCst);
        let (mut idle, mut reply) = (false, None::<Value>);
        let mut interrupted = false;

        while !(idle && reply.is_some()) {
            if self.interrupted.swap(false, Ordering::SeqCst) {
                interrupted = true;
                client.send(Channel::Control, "interrupt_request", json!({}))?;
            }
            if self.kernel_gone_with(&client) {
                return Ok(Outcome::KernelGone);
            }
            let channels = [Channel::Iopub, Channel::Stdin, Channel::Shell, Channel::Control];
            let Some((channel, msg)) = client.poll(&channels, Duration::from_millis(100))? else {
                continue;
            };
            let ours = msg.parent_id() == Some(msg_id.as_str());
            match (channel, msg.msg_type()) {
                (Channel::Iopub, "status") if ours => {
                    idle = msg.content["execution_state"] == "idle";
                },
                (Channel::Iopub, _) => self.handle_iopub(&msg),
                (Channel::Stdin, "input_request") => interrupted |= self.handle_input_request(&client, &msg)?,
                (Channel::Stdin, "rpc_request") => self.decline_rpc(&client, &msg)?,
                (Channel::Shell, "execute_reply") if ours => reply = Some(msg.content),
                _ => {},
            }
        }
        let error = reply.is_some_and(|r| r["status"] != "ok");
        Ok(Outcome::Done { error, interrupted })
    }

    fn handle_iopub(&mut self, msg: &Message) {
        let c = &msg.content;
        match msg.msg_type() {
            "stream" => {
                let text = c["text"].as_str().unwrap_or_default();
                if c["name"] == "stderr" {
                    self.write(text, Some(Color::LightRed.normal()));
                } else {
                    self.write(text, None);
                }
            },
            "execute_result" | "display_data" => {
                let data = &c["data"];
                if let Some(text) = data["text/plain"].as_str() {
                    self.write(text, None);
                } else if let Some(mime) = data.as_object().and_then(|d| d.keys().next()) {
                    self.write(&format!("[{mime} output: terminal rendering not implemented yet]"), Some(Color::DarkGray.normal()));
                }
                self.finish_line();
            },
            "error" => {
                let traceback: Vec<&str> = c["traceback"]
                    .as_array()
                    .map(|t| t.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                self.finish_line();
                let evalue = c["evalue"].as_str().unwrap_or_default();
                self.write(evalue, Some(Color::LightRed.normal()));
                self.finish_line();
                if traceback.len() > 1 {
                    self.write(&traceback.join("\n"), Some(Color::DarkGray.normal()));
                    self.finish_line();
                }
            },
            "comm_msg" if self.ui_comm.as_deref() == c["comm_id"].as_str() => {
                let data = &c["data"];
                match data["method"].as_str() {
                    Some("prompt_state") => {
                        let p = &data["params"];
                        if let Some(input) = p["input_prompt"].as_str() {
                            self.prompt.input = input.to_string();
                        }
                        if let Some(cont) = p["continuation_prompt"].as_str() {
                            self.prompt.continuation = cont.to_string();
                        }
                    },
                    Some("show_message") => {
                        let text = data["params"]["message"].as_str().unwrap_or_default();
                        self.finish_line();
                        self.write(text, Some(Color::Cyan.normal()));
                        self.finish_line();
                    },
                    _ => {},
                }
            },
            _ => {},
        }
    }

    /// Answers a `readline()`-style request. Returns true if the user interrupted.
    fn handle_input_request(&mut self, client: &KernelClient, msg: &Message) -> Result<bool> {
        let prompt = msg.content["prompt"].as_str().unwrap_or_default();
        let password = msg.content["password"].as_bool().unwrap_or(false);
        // Re-draw any partial output line (e.g. `cat("Name? ")`) as part of the prompt.
        let full_prompt = format!("{}{}", std::mem::take(&mut self.partial_line), prompt);
        if !full_prompt.is_empty() {
            print!("\r\x1b[2K");
        }

        let value = match self.input_editor.as_mut() {
            Some(_) if password => read_password(&full_prompt)?,
            Some(editor) => match editor.read_line(&InputPrompt(full_prompt))? {
                Signal::Success(line) => Some(line),
                Signal::CtrlD => Some(String::new()),
                _ => None,
            },
            // Piped mode: there is no one to answer.
            None => Some(String::new()),
        };
        match value {
            Some(value) => {
                client.send_with_parent(Channel::Stdin, "input_reply", json!({ "value": value }), msg.header.clone())?;
                Ok(false)
            },
            None => {
                client.send(Channel::Control, "interrupt_request", json!({}))?;
                Ok(true)
            },
        }
    }

    /// Positron frontend RPCs (e.g. `View()`, `rstudioapi` shims) arrive on
    /// stdin and block R until answered, so reply with an error.
    fn decline_rpc(&mut self, client: &KernelClient, msg: &Message) -> Result<()> {
        let method = msg.content["method"].as_str().unwrap_or("this request");
        client.send_with_parent(
            Channel::Stdin,
            "rpc_reply",
            json!({ "error": { "code": -32601, "message": format!("`{method}` is not supported in the terminal console") } }),
            msg.header.clone(),
        )?;
        Ok(())
    }

    fn drain_iopub(&mut self) -> Result<()> {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        while let Some((_, msg)) = client.poll(&[Channel::Iopub], Duration::ZERO)? {
            self.handle_iopub(&msg);
        }
        Ok(())
    }

    fn sync_width(&mut self) {
        let width = terminal_width();
        if width == self.width {
            return;
        }
        self.width = width;
        let Some(comm_id) = self.ui_comm.clone() else { return };
        let client = self.client.lock().unwrap();
        let _ = client.send(
            Channel::Shell,
            "comm_msg",
            json!({
                "comm_id": comm_id,
                "data": {
                    "jsonrpc": "2.0",
                    "id": uuid::Uuid::new_v4().to_string(),
                    "method": "call_method",
                    "params": { "method": "setConsoleWidth", "params": [width] },
                },
            }),
        );
    }

    fn write(&mut self, text: &str, style: Option<Style>) {
        if text.is_empty() {
            return;
        }
        let mut out = std::io::stdout().lock();
        let _ = match style {
            Some(style) => write!(out, "{}", style.paint(text)),
            None => write!(out, "{text}"),
        };
        let _ = out.flush();
        match text.rfind('\n') {
            Some(i) => self.partial_line = text[i + 1..].to_string(),
            None => self.partial_line.push_str(text),
        }
    }

    fn finish_line(&mut self) {
        if !self.partial_line.is_empty() {
            println!();
            self.partial_line.clear();
        }
    }

    fn kernel_exited(&mut self) -> bool {
        match &mut self.origin {
            Origin::Spawned(k) => k.has_exited(),
            Origin::Attached => false,
        }
    }

    fn kernel_gone_with(&mut self, client: &KernelClient) -> bool {
        self.kernel_exited() || !client.is_alive()
    }

    fn kernel_gone(&mut self) -> bool {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        self.kernel_gone_with(&client)
    }

    fn log_hint(&self) -> String {
        match &self.origin {
            Origin::Spawned(k) => format!(" (see {})", k.log_file.display()),
            Origin::Attached => String::new(),
        }
    }

    /// A kernel we spawned is shut down; an attached kernel is left running.
    /// Returns false (and says so) if the R session died rather than exiting.
    fn shutdown(&mut self) -> bool {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        match &mut self.origin {
            Origin::Spawned(kernel) => {
                if !kernel.has_exited() {
                    let _ = client.send(Channel::Control, "shutdown_request", json!({ "restart": false }));
                    kernel.stop(Duration::from_secs(3));
                }
                kernel.clean_exit = kernel.exited_successfully();
                if !kernel.clean_exit {
                    let msg = format!("R session ended abnormally; ark log kept at {}", kernel.log_file.display());
                    eprintln!("{}", Color::LightRed.paint(msg));
                }
                kernel.clean_exit
            },
            Origin::Attached => {
                let alive = client.is_alive();
                if !alive {
                    eprintln!("{}", Color::LightRed.paint("The attached kernel stopped responding."));
                }
                alive
            },
        }
    }
}

fn terminal_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80)
}

fn history_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BETTERR_HISTORY") {
        return Some(PathBuf::from(path));
    }
    let dir = dirs::data_dir()?.join("betterr");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("history.txt"))
}

/// Reads a line without echo. Returns None on Ctrl-C.
fn read_password(prompt: &str) -> Result<Option<String>> {
    use crossterm::event::{read, Event, KeyEvent, KeyEventKind};
    print!("{prompt}");
    std::io::stdout().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut value = String::new();
    let result = loop {
        if let Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) = read()? {
            match code {
                KeyCode::Enter => break Some(value),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break None,
                KeyCode::Char(ch) => value.push(ch),
                KeyCode::Backspace => {
                    value.pop();
                },
                _ => {},
            }
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    println!();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{is_browser_prompt, split_top_level};

    #[test]
    fn splits_top_level_expressions() {
        assert_eq!(split_top_level("Sys.sleep(5); print(1)"), vec!["Sys.sleep(5)", "print(1)"]);
        assert_eq!(
            split_top_level("f <- function() {\n  a; b\n}\n# note\nf()"),
            vec!["f <- function() {\n  a; b\n}", "f()"]
        );
        // Unparseable input goes through whole so R reports the error.
        assert_eq!(split_top_level("x <- ; y"), vec!["x <- ; y"]);
        // Browser commands like `n` and empty input stay intact.
        assert_eq!(split_top_level("n"), vec!["n"]);
        assert_eq!(split_top_level(""), vec![""]);
    }

    #[test]
    fn detects_browser_prompts() {
        assert!(is_browser_prompt("Browse[1]> "));
        assert!(is_browser_prompt("Browse[12]> "));
        assert!(!is_browser_prompt("> "));
        assert!(!is_browser_prompt("Browse[]> "));
    }
}
