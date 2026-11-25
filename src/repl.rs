//! The interactive loop: reedline input, Jupyter execution, output rendering.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use nu_ansi_term::{Color, Style};
use reedline::{
    default_emacs_keybindings, DefaultHinter, EditCommand, Emacs, FileBackedHistory, Highlighter, IdeMenu,
    HistoryItem, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, ValidationResult, Validator,
};
use serde_json::{json, Value};

use crate::assist::{AutoCompleteMode, CompletionMenu, Docs, SignatureLine, COMPLETION_MENU};
use crate::highlight::RHighlighter;
use crate::ide::Ide;
use crate::jupyter::{Channel, KernelClient, Message};
use crate::kernel::SpawnedKernel;
use crate::lsp::{CompletionBackend, LspClient, LspCompleter};
use crate::plots::{
    detect_image_protocol, inline_image, ImageProtocol, PlotEvent, PlotRelay, HELP_COMM_TARGET, VARIABLES_COMM_TARGET,
};

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
    /// Attached to a running kernel through this connection file.
    Attached(PathBuf),
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
    ide: Option<Arc<Ide>>,
    dap_comm: Option<String>,
    /// Raised to make `read_line` yield to the main loop (IDE request,
    /// debugger command, or output from another client).
    wake: Arc<AtomicBool>,
    /// IOPub messages collected by the idle pump while the editor waits.
    idle_inbox: Arc<Mutex<VecDeque<Message>>>,
    queue: VecDeque<Queued>,
    /// We yielded mid-edit, so the prompt line is still drawn.
    prompt_on_screen: bool,
    echo_highlighter: RHighlighter,
    /// The running code was typed here (not sent by the IDE), so a debugger
    /// stop it causes should leave keyboard focus in the console.
    running_typed_input: bool,
    plots: Arc<PlotRelay>,
    image_protocol: Option<ImageProtocol>,
    /// Output starting with one of R's debugger lines (`Called from:`, `debug
    /// at`, ...), held until the prompt shows whether R really stopped in
    /// the browser, and since when.
    held_debug: Option<(String, Instant)>,
}

/// How R introduces the lines it prints when the browser stops (`debugging
/// in:` comes first when a function flagged with `debug()` is called).
const DEBUG_PREFIXES: [&str; 4] = ["Called from: ", "debugging in: ", "debug at ", "debug: "];

/// Held output that no browser prompt claimed within this time is the user's own.
const DEBUG_HOLD: Duration = Duration::from_secs(1);

/// Ark hides R's debugger lines, since Positron shows the location in the
/// editor. A terminal console needs them to follow along, so they're turned
/// back on, unless the user set the option themselves.
const SHOW_DEBUG_LINES: &str = "if (is.null(getOption(\"ark.debugger.filter_debug_output\"))) \
options(ark.debugger.filter_debug_output = FALSE)";

/// Code the console runs on someone else's behalf: the IDE's Run Selection or
/// a debugger step clicked in the IDE. It is echoed as if typed.
struct Queued {
    code: String,
    location: Option<Value>,
    remember: bool,
}

enum Outcome {
    Done { error: bool, interrupted: bool },
    KernelGone,
}

/// A top-level expression and its (row, byte column) extent in the input.
#[derive(Debug, PartialEq)]
struct Chunk {
    text: String,
    start: (usize, usize),
    end: (usize, usize),
}

fn end_point(code: &str) -> (usize, usize) {
    let rows = code.matches('\n').count();
    let last = code.rsplit('\n').next().unwrap_or("");
    (rows, last.len())
}

/// Shifts a chunk's position by the location of the whole input, giving the
/// file location ark uses for srcrefs and breakpoints (columns in UTF-8 bytes).
fn chunk_location(location: &Value, chunk: &Chunk) -> Value {
    let base_line = location["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
    let base_char = location["range"]["start"]["character"].as_u64().unwrap_or(0) as usize;
    let shift = |(row, col): (usize, usize)| {
        json!({ "line": base_line + row, "character": if row == 0 { base_char + col } else { col } })
    };
    json!({ "uri": location["uri"], "range": { "start": shift(chunk.start), "end": shift(chunk.end) } })
}

/// Splits input into top-level R expressions. ark keeps evaluating the
/// remaining expressions of a request after an interrupt, so the console sends
/// them one at a time and stops on interrupt or error, like R's own console.
/// Input that doesn't parse is sent whole so R reports the syntax error.
fn split_top_level(code: &str) -> Vec<Chunk> {
    let whole = || {
        vec![Chunk {
            text: code.to_string(),
            start: (0, 0),
            end: end_point(code),
        }]
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&tree_sitter_r::LANGUAGE.into()).is_err() {
        return whole();
    }
    let Some(tree) = parser.parse(code, None) else {
        return whole();
    };
    let root = tree.root_node();
    if root.has_error() {
        return whole();
    }
    let mut cursor = root.walk();
    let chunks: Vec<Chunk> = root
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .map(|n| Chunk {
            text: code[n.byte_range()].to_string(),
            start: (n.start_position().row, n.start_position().column),
            end: (n.end_position().row, n.end_position().column),
        })
        .collect();
    if chunks.is_empty() { whole() } else { chunks }
}

/// True for IOPub traffic that should interrupt line editing: debugger
/// commands from the IDE, and output produced on behalf of another client.
fn needs_main_loop(msg: &Message, dap_comm: Option<&str>) -> bool {
    match msg.msg_type() {
        "comm_msg" => dap_comm.is_some() && msg.content["comm_id"].as_str() == dap_comm,
        "stream" | "execute_result" | "display_data" | "error" => true,
        _ => false,
    }
}

impl Repl {
    pub fn start(
        client: KernelClient,
        origin: Origin,
        use_lsp: bool,
        interrupted: Arc<AtomicBool>,
        ide: Option<(u16, String)>,
    ) -> Result<Self> {
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
            ide: None,
            dap_comm: None,
            wake: Arc::new(AtomicBool::new(false)),
            idle_inbox: Arc::new(Mutex::new(VecDeque::new())),
            queue: VecDeque::new(),
            prompt_on_screen: false,
            echo_highlighter: RHighlighter::new(),
            running_typed_input: false,
            plots: Arc::new(PlotRelay::new(None)),
            image_protocol: detect_image_protocol(),
            held_debug: None,
        };
        let info = repl.wait_ready(Duration::from_secs(60))?;
        repl.open_ui_comm()?;
        repl.run_silently(SHOW_DEBUG_LINES)?;
        let interactive = std::io::stdin().is_terminal();

        if let Some((port, token)) = ide {
            let ide = Ide::connect(port, &token, repl.wake.clone())?;
            repl.ide = Some(ide.clone());
            repl.plots = Arc::new(PlotRelay::new(Some(ide.clone())));
            repl.open_debugger(&ide)?;
        }
        if let Some(ui) = &repl.ui_comm {
            repl.plots.set_ui_comm(ui);
        }
        // Opening the help comm starts R's help server and ark's help proxy;
        // help pages then arrive as `show_help` events.
        let help = repl.client.lock().unwrap().open_comm(HELP_COMM_TARGET, json!({}))?;
        repl.plots.set_help_comm(&help);
        // The IDE's environment explorer: ark sends the variables of the
        // global env (or the debug frame) and serves `inspect`, `delete`, ...
        if repl.ide.is_some() {
            let variables = repl.client.lock().unwrap().open_comm(VARIABLES_COMM_TARGET, json!({}))?;
            repl.plots.set_variables_comm(&variables);
        }

        let backend = if let Some(ide) = &repl.ide {
            // Hosted: the IDE owns the kernel's one LSP connection.
            CompletionBackend::Ide(ide.clone())
        } else if use_lsp && interactive {
            match repl.open_lsp() {
                Ok(lsp) => CompletionBackend::Lsp(Arc::new(Mutex::new(lsp))),
                Err(err) => {
                    eprintln!("{}", Color::Yellow.paint(format!("completion unavailable: {err:#}")));
                    CompletionBackend::None
                },
            }
        } else {
            CompletionBackend::None
        };

        if interactive {
            let r_version = info["language_info"]["version"].as_str().unwrap_or("R");
            let ark = info["implementation_version"].as_str().unwrap_or("?");
            let how = match &repl.origin {
                Origin::Spawned(_) => "new session",
                Origin::Attached(_) => "attached to running session",
            };
            let kitty = reedline::kitty_protocol_available();
            let newline = if shift_enter_distinct(kitty) { "Shift-Enter" } else { "Alt-Enter" };
            let dim = Color::DarkGray.normal();
            println!("{}", Color::Blue.bold().paint(r_version.trim()));
            println!("{}", dim.paint(format!("ark {ark} · {how}")));
            println!("{}", dim.paint(format!("Tab completes · {newline} newline · Ctrl-C interrupts · Ctrl-D quits")));
            repl.replay_startup_output();
            repl.editor = Some(repl.build_editor(backend, kitty)?);
            repl.spawn_idle_pump();
            repl.input_editor = Some(Reedline::create());
        }
        Ok(repl)
    }

    /// Shows what R printed while the session started (e.g. `.Rprofile`, renv),
    /// as recorded by the kernel. A spawned kernel's console saw it live; one
    /// that attached later replays it.
    fn replay_startup_output(&mut self) {
        let Origin::Attached(connection_file) = &self.origin else {
            return;
        };
        for (stream, text) in crate::startup::read(connection_file) {
            let style = (stream == "stderr").then(|| Color::LightRed.normal());
            self.write(&text, style);
        }
        self.finish_line();
    }

    /// `kitty`: whether the terminal supports the kitty keyboard protocol
    /// (asked once at startup, see `kitty_protocol_available`).
    fn build_editor(&self, backend: CompletionBackend, kitty: bool) -> Result<Reedline> {
        let mut keybindings = default_emacs_keybindings();
        // Completion pops up as you type; Tab takes the highlighted suggestion
        // (or asks for suggestions when none are showing), Enter always runs.
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuAccept, ReedlineEvent::Menu(COMPLETION_MENU.into())]),
        );
        keybindings.add_binding(KeyModifiers::SHIFT, KeyCode::BackTab, ReedlineEvent::MenuPrevious);
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Enter,
            ReedlineEvent::Multiple(vec![ReedlineEvent::Esc, ReedlineEvent::Enter]),
        );
        keybindings.add_binding(
            KeyModifiers::ALT,
            KeyCode::Enter,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );

        let tab_pressed = Arc::new(AtomicBool::new(false));
        let menu_openings = Arc::new(AtomicU64::new(0));
        let docs = Arc::new(Mutex::new(Docs::default()));
        let menu = IdeMenu::default()
            .with_name(COMPLETION_MENU)
            .with_marker("")
            // A typed character that can't extend the name (space, `(`, ...) closes it.
            .with_word_chars("._")
            .with_default_border()
            .with_max_completion_height(10);

        let mut editor = Reedline::create()
            .with_highlighter(Box::new(RHighlighter::new()))
            .with_validator(Box::new(KernelValidator {
                client: self.client.clone(),
            }))
            .with_completer(Box::new(LspCompleter::new(backend.clone(), docs.clone(), tab_pressed.clone(), menu_openings.clone())))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(CompletionMenu::new(
                menu,
                menu_openings,
                docs,
                backend.clone(),
            ))))
            // Refreshes an open menu as you type; `CompletionMenu` keeps a lone suggestion from being inserted.
            .with_quick_completions(true)
            .with_partial_completions(false)
            .with_status_line(Box::new(SignatureLine::new(backend)))
            .with_hinter(Box::new(DefaultHinter::default().with_style(Style::new().fg(Color::DarkGray))))
            .with_edit_mode(Box::new(AutoCompleteMode::new(Emacs::new(keybindings), tab_pressed)))
            .use_bracketed_paste(true)
            // Lets terminals that support it report Shift-Enter (bound by reedline
            // to a newline, like Alt-Enter) apart from Enter.
            .use_kitty_keyboard_enhancement(kitty)
            .with_break_signal(self.wake.clone());
        if let Some(path) = history_path() {
            editor = editor.with_history(Box::new(FileBackedHistory::with_file(10_000, path)?));
        }
        Ok(editor)
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<Value> {
        let client = self.client.clone();
        let mut early = Vec::new();
        let info = client.lock().unwrap().wait_ready(
            timeout,
            || self.kernel_exited().then(|| format!("kernel exited during startup{}", self.exit_details())),
            |msg| early.push(msg),
        )?;
        // Output R printed while starting (`.Rprofile`) can arrive before the
        // kernel answers. A spawned kernel's console shows it as it does any
        // output. An attached console replays the recorded copy instead (see
        // `replay_startup_output`), so showing this too would print it twice.
        if matches!(self.origin, Origin::Spawned(_)) {
            for msg in &early {
                self.handle_iopub(msg);
            }
        }
        Ok(info)
    }

    /// Runs R code that prints nothing and isn't kept in history.
    fn run_silently(&mut self, code: &str) -> Result<()> {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        let request = json!({
            "code": code,
            "silent": true,
            "store_history": false,
            "user_expressions": {},
            "allow_stdin": false,
            "stop_on_error": false,
        });
        let msg_id = client.send(Channel::Shell, "execute_request", request)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match client.poll(&[Channel::Iopub, Channel::Shell], Duration::from_millis(100))? {
                Some((Channel::Shell, msg)) if msg.parent_id() == Some(msg_id.as_str()) => break,
                Some((Channel::Iopub, msg)) if msg.parent_id() != Some(msg_id.as_str()) => self.handle_iopub(&msg),
                _ => {},
            }
        }
        Ok(())
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

    /// While the editor waits, a background thread moves IOPub traffic into
    /// `idle_inbox` and wakes the main loop for anything that must be acted on
    /// or shown. It only ever `try_lock`s the client, so it never competes with
    /// an execute (which holds the lock throughout). Not a reedline idle
    /// callback: those force a cursor-position query on every repaint.
    fn spawn_idle_pump(&self) {
        let client = Arc::downgrade(&self.client);
        let inbox = self.idle_inbox.clone();
        let wake = self.wake.clone();
        let dap_comm = self.dap_comm.clone();
        let plots = self.plots.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(50));
                let Some(client) = client.upgrade() else { return };
                let Ok(client) = client.try_lock() else { continue };
                // Hosted plot traffic is relayed right here, without
                // interrupting whatever is being typed at the prompt.
                plots.send_requests(&client);
                while let Ok(Some((_, msg))) = client.poll(&[Channel::Iopub], Duration::ZERO) {
                    if plots.hosted() && !matches!(plots.handle(&msg), PlotEvent::None) {
                        continue;
                    }
                    if needs_main_loop(&msg, dap_comm.as_deref()) {
                        wake.store(true, Ordering::SeqCst);
                    }
                    inbox.lock().unwrap().push_back(msg);
                }
            }
        });
    }

    /// Opens ark's debugger comm and tells the IDE which port to attach to.
    /// Holding the comm makes this console the one that runs debugger
    /// commands (`n`, `c`, ...) clicked in the IDE.
    fn open_debugger(&mut self, ide: &Ide) -> Result<()> {
        let client = self.client.clone();
        let (comm_id, port) = client
            .lock()
            .unwrap()
            .start_server_comm("ark_dap", Duration::from_secs(15), |msg| self.handle_iopub(msg))?;
        self.dap_comm = Some(comm_id);
        ide.send(json!({ "type": "dap_port", "port": port }));
        Ok(())
    }

    fn open_lsp(&mut self) -> Result<LspClient> {
        let client = self.client.clone();
        let (_, port) = client
            .lock()
            .unwrap()
            .start_server_comm("lsp", Duration::from_secs(15), |msg| self.handle_iopub(msg))?;
        LspClient::connect(port, &std::env::current_dir()?)
    }

    pub fn run(&mut self) -> Result<()> {
        loop {
            self.pump()?;
            if let Some(job) = self.queue.pop_front() {
                if job.remember {
                    self.remember(&job.code);
                }
                if let Outcome::KernelGone = self.execute_all(&job.code, job.location.as_ref(), true)? {
                    break;
                }
                continue;
            }
            self.sync_width();
            self.finish_line();
            if self.kernel_gone() {
                break;
            }
            let prompt = self.prompt.clone();
            // After a yield, reedline keeps the partially typed buffer itself.
            let editor = self.editor.as_mut().expect("interactive editor");
            match editor.read_line(&prompt)? {
                Signal::Success(code) => {
                    // An empty line is meaningful in the debugger (it steps).
                    if code.trim().is_empty() && !prompt.is_browser() {
                        continue;
                    }
                    self.running_typed_input = true;
                    let outcome = self.execute_all(&code, None, false);
                    self.running_typed_input = false;
                    if let Outcome::KernelGone = outcome? {
                        break;
                    }
                },
                Signal::ExternalBreak(_) => self.prompt_on_screen = true,
                Signal::CtrlD => break,
                _ => continue,
            }
        }
        self.finish_line();
        self.shutdown();
        Ok(())
    }

    /// Handles IOPub traffic gathered while idle, then IDE requests.
    fn pump(&mut self) -> Result<()> {
        let gathered: Vec<Message> = self.idle_inbox.lock().unwrap().drain(..).collect();
        for msg in &gathered {
            self.handle_iopub(msg);
        }
        self.drain_iopub()?;
        // Another client's code: its prompt has arrived along with its output.
        self.release_debug(self.prompt.is_browser());
        let events = self.ide.as_ref().map(|ide| ide.take_events()).unwrap_or_default();
        for event in events {
            if event["type"] == "incompatible" {
                // The IDE speaks another protocol version and has closed the
                // channel; the console itself keeps working.
                let text = event["message"].as_str().unwrap_or("The IDE can't use this version of better.");
                self.clear_prompt();
                self.finish_line();
                self.write(text, Some(Color::Red.normal()));
                self.finish_line();
            } else if event["type"] == "execute"
                && let Some(code) = event["code"].as_str()
            {
                self.queue.push_back(Queued {
                    code: code.to_string(),
                    location: event.get("location").filter(|l| !l.is_null()).cloned(),
                    remember: true,
                });
            }
        }
        Ok(())
    }

    /// Shows queued code after the prompt, highlighted, as if it were typed.
    fn echo(&mut self, code: &str) {
        self.clear_prompt();
        self.finish_line();
        let prompt_style = if self.prompt.is_browser() { Color::Yellow } else { Color::Blue }.normal();
        let cont_style = Color::DarkGray.normal();
        let mut out = prompt_style.paint(&self.prompt.input).to_string();
        for (style, text) in self.echo_highlighter.highlight(code, 0).buffer {
            for (i, part) in text.split('\n').enumerate() {
                if i > 0 {
                    out.push('\n');
                    out.push_str(&cont_style.paint(&self.prompt.continuation).to_string());
                }
                out.push_str(&style.paint(part).to_string());
            }
        }
        println!("{out}");
        let _ = std::io::stdout().flush();
    }

    /// Draws a plot inline if the terminal can show images.
    fn show_image(&mut self, png_base64: &str) {
        self.clear_prompt();
        self.finish_line();
        match self.image_protocol {
            Some(protocol) => match inline_image(protocol, png_base64) {
                Ok(seq) => {
                    println!("{seq}");
                    let _ = std::io::stdout().flush();
                },
                Err(err) => {
                    self.write(&format!("[plot: can't draw it: {err}]"), Some(Color::DarkGray.normal()));
                    self.finish_line();
                },
            },
            None => {
                let hint = "[plot: this terminal can't show images; use iTerm2, WezTerm, kitty or \
                            Windows Terminal, set BETTER_INLINE_IMAGES, or the VS Code extension's plot pane]";
                self.write(hint, Some(Color::DarkGray.normal()));
                self.finish_line();
            },
        }
    }

    /// Standalone: interactive output (htmlwidgets, Shiny, help, files) goes to
    /// `$BROWSER`, or the system's default opener.
    fn open_externally(&mut self, target: &str, title: &str) {
        let ok = match std::env::var("BROWSER").ok().filter(|b| !b.is_empty()) {
            Some(browser) => run_quietly(&browser, target),
            None => open_with_system(target),
        };
        self.clear_prompt();
        self.finish_line();
        let note = if ok {
            format!("[{title}: opened in your browser]")
        } else {
            format!("[{title}: open {target}]")
        };
        self.write(&note, Some(Color::DarkGray.normal()));
        self.finish_line();
    }

    fn remember(&mut self, code: &str) {
        if let Some(editor) = self.editor.as_mut() {
            let _ = editor.history_mut().save(HistoryItem::from_command_line(code));
        }
    }

    /// Erases the prompt line left on screen after yielding mid-edit.
    fn clear_prompt(&mut self) {
        if std::mem::take(&mut self.prompt_on_screen) {
            print!("\r\x1b[2K");
        }
    }

    /// Non-interactive mode: run piped input, stopping at the first error.
    /// Returns false on an R error or if the R session died.
    pub fn run_script(&mut self, code: &str) -> Result<bool> {
        let ok = match self.execute_all(code, None, false)? {
            Outcome::Done { error, .. } => !error,
            Outcome::KernelGone => true,
        };
        self.finish_line();
        let clean = self.shutdown();
        Ok(ok && clean)
    }

    /// Runs each top-level expression in turn. `echo` shows each one after the
    /// prompt first, for code that wasn't typed here.
    fn execute_all(&mut self, code: &str, location: Option<&Value>, echo: bool) -> Result<Outcome> {
        let mut last = Outcome::Done { error: false, interrupted: false };
        for chunk in split_top_level(code) {
            if echo {
                self.echo(&chunk.text);
            }
            let location = location.map(|l| chunk_location(l, &chunk));
            last = self.execute(&chunk.text, location)?;
            if !matches!(last, Outcome::Done { error: false, interrupted: false }) {
                break;
            }
        }
        Ok(last)
    }

    /// Runs one request. A file `location` gives R real srcrefs, so the
    /// debugger shows the editor's file and IDE breakpoints apply.
    fn execute(&mut self, code: &str, location: Option<Value>) -> Result<Outcome> {
        let client = self.client.clone();
        let client = client.lock().unwrap();
        let mut request = json!({
            "code": code,
            "silent": false,
            "store_history": true,
            "user_expressions": {},
            "allow_stdin": true,
            "stop_on_error": true,
        });
        if let Some(location) = location {
            request["positron"] = json!({ "code_location": location });
        }
        let msg_id = client.send(Channel::Shell, "execute_request", request)?;
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
            if self.held_debug.as_ref().is_some_and(|(_, since)| since.elapsed() > DEBUG_HOLD) {
                self.release_debug(false);
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
        // The prompt R stopped at (from the UI comm) settles any held debugger lines.
        self.release_debug(self.prompt.is_browser());
        let error = reply.is_some_and(|r| r["status"] != "ok");
        Ok(Outcome::Done { error, interrupted })
    }

    fn handle_iopub(&mut self, msg: &Message) {
        match self.plots.handle(msg) {
            PlotEvent::None => {},
            PlotEvent::Handled => return,
            PlotEvent::Image(png) => return self.show_image(&png),
            PlotEvent::Open { target, title } => return self.open_externally(&target, &title),
            PlotEvent::Note(note) => {
                self.clear_prompt();
                self.finish_line();
                self.write(note, Some(Color::DarkGray.normal()));
                return self.finish_line();
            },
        }
        let c = &msg.content;
        match msg.msg_type() {
            "stream" => {
                let text = c["text"].as_str().unwrap_or_default();
                if c["name"] == "stderr" {
                    self.write(text, Some(Color::LightRed.normal()));
                } else {
                    self.write_stdout(text);
                }
            },
            "execute_result" | "display_data" => {
                let data = &c["data"];
                if let Some(png) = data["image/png"].as_str() {
                    self.show_image(png);
                } else if let Some(text) = data["text/plain"].as_str() {
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
            "comm_msg" if self.dap_comm.is_some() && self.dap_comm.as_deref() == c["comm_id"].as_str() => {
                let data = &c["data"];
                match data["method"].as_str() {
                    Some(state @ ("start_debug" | "stop_debug")) => {
                        let state = if state == "start_debug" { "start" } else { "stop" };
                        if let Some(ide) = &self.ide {
                            let origin = if self.running_typed_input { "console" } else { "ide" };
                            ide.send(json!({ "type": "debug", "state": state, "origin": origin }));
                        }
                    },
                    // A step/continue clicked in the IDE, to be run at the browser prompt.
                    Some("execute") => {
                        if let Some(command) = data["params"]["command"].as_str() {
                            self.queue.push_back(Queued {
                                code: command.to_string(),
                                location: None,
                                remember: false,
                            });
                        }
                    },
                    _ => {},
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
        self.release_debug(false);
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
        // Anything else printed after them means they weren't debugger lines:
        // R prints those last, right before the browser prompt.
        self.release_debug(false);
        self.emit(text, style);
    }

    /// R's standard output. From a line that starts like a debugger line on,
    /// it's held until the prompt shows what it was.
    fn write_stdout(&mut self, text: &str) {
        if let Some((held, _)) = &mut self.held_debug {
            held.push_str(text);
            return;
        }
        match debug_lines_start(text, self.partial_line.is_empty()) {
            Some(i) => {
                self.emit(&text[..i], None);
                self.held_debug = Some((text[i..].to_string(), Instant::now()));
            },
            None => self.emit(text, None),
        }
    }

    /// Prints held output: dimmed, without ark's own stepping lines, if R
    /// stopped in the browser (`at_browser`), otherwise as it came.
    fn release_debug(&mut self, at_browser: bool) {
        let Some((text, _)) = self.held_debug.take() else {
            return;
        };
        if at_browser {
            self.emit(&shown_debug_lines(&text), Some(Color::DarkGray.normal()));
        } else {
            self.emit(&text, None);
        }
    }

    fn emit(&mut self, text: &str, style: Option<Style>) {
        if text.is_empty() {
            return;
        }
        self.clear_prompt();
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
            Origin::Attached(_) => false,
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

    /// Why a kernel we spawned stopped, from its log, and where the log is.
    fn exit_details(&self) -> String {
        match &self.origin {
            Origin::Spawned(k) => match crate::arklog::failure_reason_in(&k.log_file) {
                Some(reason) => format!(": {reason} (log: {})", k.log_file.display()),
                None => format!(" (see {})", k.log_file.display()),
            },
            Origin::Attached(_) => String::new(),
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
                    let reason = crate::arklog::failure_reason_in(&kernel.log_file)
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default();
                    let msg = format!("R session ended abnormally{reason}; ark log kept at {}", kernel.log_file.display());
                    eprintln!("{}", Color::LightRed.paint(msg));
                }
                kernel.clean_exit
            },
            Origin::Attached(_) => {
                let alive = client.is_alive();
                if !alive {
                    eprintln!("{}", Color::LightRed.paint("The attached kernel stopped responding."));
                }
                alive
            },
        }
    }
}

/// Runs `program target` without showing its output; true if it succeeded.
fn run_quietly(program: &str, target: &str) -> bool {
    std::process::Command::new(program)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Opens a URL or file with the system's default handler.
#[cfg(not(windows))]
fn open_with_system(target: &str) -> bool {
    run_quietly(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }, target)
}

/// Opens a URL or file with the system's default handler. Not `cmd /C start`:
/// cmd.exe would parse the target, and a `&` in a URL starts a new command.
/// The `open` crate passes it to PowerShell as data (an environment variable).
#[cfg(windows)]
fn open_with_system(target: &str) -> bool {
    open::that(target).is_ok()
}

/// Whether Shift-Enter reaches the console apart from Enter (and so inserts a
/// newline), given whether the terminal supports the kitty keyboard protocol.
/// Always on Windows, where keys come through the console API. Elsewhere other
/// terminals (e.g. VTE's: GNOME Terminal, Ptyxis) send it as plain Enter,
/// unless the host says it maps Shift-Enter itself (`BETTER_SHIFT_ENTER=1`;
/// the VS Code extension sends Alt-Enter's sequence for it).
fn shift_enter_distinct(kitty: bool) -> bool {
    cfg!(windows) || kitty || std::env::var("BETTER_SHIFT_ENTER").is_ok_and(|v| v == "1")
}

fn terminal_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80)
}

fn history_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BETTER_HISTORY") {
        return Some(PathBuf::from(path));
    }
    let dir = dirs::data_dir()?.join("better");
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

/// Where output starts to look like R's debugger lines: a line starting with
/// one of their prefixes (`at_line_start`: the text starts a new line).
fn debug_lines_start(text: &str, at_line_start: bool) -> Option<usize> {
    let mut line_starts = at_line_start.then_some(0).into_iter().chain(text.match_indices('\n').map(|(i, _)| i + 1));
    line_starts.find(|&i| DEBUG_PREFIXES.iter().any(|p| text[i..].starts_with(p)))
}

/// The debugger lines worth showing. Ark steps over the calls it injects for
/// editor breakpoints (`.ark_auto_step()`, `.ark_breakpoint()`), and R prints
/// a line for each of those steps too; they're left out.
fn shown_debug_lines(text: &str) -> String {
    let mut messages: Vec<String> = Vec::new();
    for line in text.split_inclusive('\n') {
        match messages.last_mut() {
            Some(message) if !DEBUG_PREFIXES.iter().any(|p| line.starts_with(p)) => message.push_str(line),
            _ => messages.push(line.to_string()),
        }
    }
    messages.retain(|m| !m.contains(".ark_auto_step(") && !m.contains(".ark_breakpoint("));
    let cwd = std::env::current_dir().ok();
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    messages.iter().map(|m| short_debug_location(m, cwd.as_deref(), home.as_deref())).collect()
}

/// `debug at file:///home/me/project/R/a.R#4: ...` as `debug at R/a.R#4: ...`:
/// code run from the editor has a `file://` URI, which reads better as a path
/// relative to the working directory (or the home directory).
fn short_debug_location(message: &str, cwd: Option<&std::path::Path>, home: Option<&std::path::Path>) -> String {
    let Some(rest) = message.strip_prefix("debug at file://") else {
        return message.to_string();
    };
    // The location ends at the first `#<line>: `.
    let Some(end) = rest.match_indices('#').map(|(i, _)| i).find(|&i| {
        let digits = rest[i + 1..].chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest[i + 1 + digits..].starts_with(": ")
    }) else {
        return message.to_string();
    };
    let decoded = percent_decode(&rest[..end]);
    // `file:///C:/x` on Windows.
    let decoded = if cfg!(windows) { decoded.trim_start_matches('/').to_string() } else { decoded };
    // Compare resolved paths too: on macOS `/var` and `/tmp` lead into `/private`.
    let path = PathBuf::from(&decoded);
    let paths = [Some(path.clone()), std::fs::canonicalize(&path).ok()];
    let under = |base: Option<&std::path::Path>| {
        let base = base?;
        let bases = [Some(base.to_path_buf()), std::fs::canonicalize(base).ok()];
        paths
            .iter()
            .flatten()
            .find_map(|p| bases.iter().flatten().find_map(|b| p.strip_prefix(b).ok().map(|r| r.display().to_string())))
    };
    let shown = if let Some(relative) = under(cwd) {
        relative
    } else if let Some(relative) = under(home) {
        format!("~/{relative}")
    } else {
        decoded
    };
    format!("debug at {shown}{}", &rest[end..])
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%').then(|| text.get(i + 1..i + 3)).flatten();
        match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(b) => {
                out.push(b);
                i += 3;
            },
            None => {
                out.push(bytes[i]);
                i += 1;
            },
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use std::path::Path;

    use super::{
        chunk_location, debug_lines_start, is_browser_prompt, short_debug_location, shown_debug_lines, split_top_level,
    };

    fn texts(code: &str) -> Vec<String> {
        split_top_level(code).into_iter().map(|c| c.text).collect()
    }

    #[test]
    fn splits_top_level_expressions() {
        assert_eq!(texts("Sys.sleep(5); print(1)"), vec!["Sys.sleep(5)", "print(1)"]);
        assert_eq!(
            texts("f <- function() {\n  a; b\n}\n# note\nf()"),
            vec!["f <- function() {\n  a; b\n}", "f()"]
        );
        // Unparseable input goes through whole so R reports the error.
        assert_eq!(texts("x <- ; y"), vec!["x <- ; y"]);
        // Browser commands like `n` and empty input stay intact.
        assert_eq!(texts("n"), vec!["n"]);
        assert_eq!(texts(""), vec![""]);
    }

    #[test]
    fn chunk_locations_are_shifted_into_the_file() {
        // Selection starts at line 10, byte column 4 of the file.
        let loc = json!({ "uri": "file:///p/a.R",
            "range": { "start": { "line": 10, "character": 4 }, "end": { "line": 12, "character": 5 } } });
        let chunks = split_top_level("a <- 1; f <- function() {\n  é\n}");
        let shifted: Vec<Value> = chunks.iter().map(|c| chunk_location(&loc, c)["range"].clone()).collect();
        assert_eq!(shifted[0], json!({ "start": { "line": 10, "character": 4 }, "end": { "line": 10, "character": 10 } }));
        // Second chunk ends on a later line: its end column is not shifted. "}" ends at byte 1.
        assert_eq!(shifted[1], json!({ "start": { "line": 10, "character": 12 }, "end": { "line": 12, "character": 1 } }));
    }

    #[test]
    fn finds_where_debugger_lines_start() {
        assert_eq!(debug_lines_start("Called from: g()\n", true), Some(0));
        assert_eq!(debug_lines_start("y is 2 \ndebug at #6: y * 2\n", true), Some(8));
        // Only at the start of a line.
        assert_eq!(debug_lines_start("Called from: g()\n", false), None);
        assert_eq!(debug_lines_start("x <- \"debug: no\"\n", true), None);
        assert_eq!(debug_lines_start("[1] 4\n", true), None);
    }

    #[test]
    fn leaves_out_arks_own_steps() {
        let held = "debug at /p/a.R#3: base::.ark_auto_step(base::.ark_breakpoint(browser(), \"u\", \"1\"))\n\
                    Called from: .ark_breakpoint(browser(), \"u\", \"1\")\n\
                    debug at /p/a.R#3: {\n    x <- 1\n}\n";
        assert_eq!(shown_debug_lines(held), "debug at /p/a.R#3: {\n    x <- 1\n}\n");
        assert_eq!(shown_debug_lines("Called from: g()\n"), "Called from: g()\n");
    }

    #[cfg(unix)]
    #[test]
    fn shortens_file_uris_in_debug_lines() {
        let (cwd, home) = (Some(Path::new("/home/me/proj")), Some(Path::new("/home/me")));
        let short = |m| short_debug_location(m, cwd, home);
        assert_eq!(short("debug at file:///home/me/proj/R/a.R#4: y * 2\n"), "debug at R/a.R#4: y * 2\n");
        assert_eq!(short("debug at file:///home/me/my%20code/a.R#12: f(x)\n"), "debug at ~/my code/a.R#12: f(x)\n");
        assert_eq!(short("debug at file:///tmp/a%23b.R#2: x\n"), "debug at /tmp/a#b.R#2: x\n");
        // Code typed at the console has no file; other lines are left alone.
        assert_eq!(short("debug at #1: a\n"), "debug at #1: a\n");
        assert_eq!(short("Called from: g()\n"), "Called from: g()\n");
    }

    #[test]
    fn detects_browser_prompts() {
        assert!(is_browser_prompt("Browse[1]> "));
        assert!(is_browser_prompt("Browse[12]> "));
        assert!(!is_browser_prompt("> "));
        assert!(!is_browser_prompt("Browse[]> "));
    }
}
