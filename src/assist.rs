//! Editor-like assistance at the prompt: completion that pops up as you type,
//! and the signature of the function call around the cursor on a status line.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent};
use nu_ansi_term::{Color, Style};
use reedline::{
    Completer, EditCommand, EditMode, Editor, EventStatus, Menu, MenuEvent, MenuSettings, Painter, PromptEditMode,
    ReedlineEvent, ReedlineRawEvent, StatusLine, Suggestion,
};
use serde_json::Value;

use crate::help_box::{beside, help_from_markdown, render_box, visible_width, wrap, Help, Side, Span, BOX_FRAME};
use crate::lsp::{kind_label, CompletionBackend};

pub const COMPLETION_MENU: &str = "completion_menu";

/// How long a signature lookup may hold up redrawing the prompt.
const SIGNATURE_TIMEOUT: Duration = Duration::from_millis(500);

/// Where the cursor is, for deciding what to offer.
#[derive(Debug, Default, PartialEq)]
pub struct CodeContext {
    /// Inside a string or a comment, where completions would be noise.
    pub in_string_or_comment: bool,
    /// The quote of the string the cursor is in, if any.
    pub open_quote: Option<char>,
    /// The innermost function call whose parentheses enclose the cursor.
    pub call: Option<Call>,
}

#[derive(Debug, PartialEq)]
pub struct Call {
    /// Byte offset of the call's `(`.
    pub open: usize,
    /// The called function, e.g. `mean` or `stats::sd`.
    pub function: String,
    /// Which argument the cursor is in (0-based, by commas).
    pub arg_index: usize,
    /// The name given to the current argument (`na.rm = ...`), if any.
    pub arg_name: Option<String>,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '.' || c == '_'
}

/// Scans R code up to `pos`: strings, comments and bracket nesting.
pub fn code_context(text: &str, pos: usize) -> CodeContext {
    // (bracket, byte offset, argument index, start of the current argument)
    let mut stack: Vec<(char, usize, usize, usize)> = Vec::new();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut chars = text[..pos].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if comment {
            comment = c != '\n';
            continue;
        }
        if let Some(q) = quote {
            if c == '\\' {
                chars.next();
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '#' => comment = true,
            '(' | '[' | '{' => stack.push((c, i, 0, i + 1)),
            ')' | ']' | '}' => {
                stack.pop();
            },
            ',' => {
                if let Some(top) = stack.last_mut() {
                    top.2 += 1;
                    top.3 = i + 1;
                }
            },
            _ => {},
        }
    }
    // Backticks quote names, not text: completing inside them is fine.
    let in_string_or_comment = comment || matches!(quote, Some('"' | '\''));
    let call = match stack.last() {
        Some(&('(', open, arg_index, arg_start)) if !in_string_or_comment => {
            let before = text[..open].trim_end();
            let start = before
                .char_indices()
                .rev()
                .take_while(|&(_, c)| is_ident(c) || c == ':')
                .last()
                .map_or(before.len(), |(i, _)| i);
            let function = before[start..].to_string();
            let keyword = matches!(function.as_str(), "if" | "for" | "while" | "function" | "switch" | "repeat");
            let named = function.chars().next().is_some_and(|c| c.is_alphabetic() || c == '.');
            (named && !keyword).then(|| {
                let arg = &text[arg_start..pos];
                let arg_name = arg
                    .split_once('=')
                    .filter(|(_, rest)| !rest.starts_with('='))
                    .map(|(name, _)| name.trim().to_string())
                    .filter(|name| !name.is_empty() && name.chars().all(is_ident));
                Call { open, function, arg_index, arg_name }
            })
        },
        _ => None,
    };
    let open_quote = quote.filter(|q| *q != '`');
    CodeContext { in_string_or_comment, open_quote, call }
}

/// `text` with whatever it leaves open (a string, brackets) closed at the end.
pub fn close_brackets(text: &str) -> String {
    let mut open: Vec<char> = Vec::new();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if comment {
            comment = c != '\n';
            continue;
        }
        if let Some(q) = quote {
            if c == '\\' {
                chars.next();
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '#' => comment = true,
            '(' => open.push(')'),
            '[' => open.push(']'),
            '{' => open.push('}'),
            ')' | ']' | '}' if open.last() == Some(&c) => {
                open.pop();
            },
            _ => {},
        }
    }
    let mut out = text.to_string();
    if comment {
        out.push('\n');
    }
    out.extend(quote);
    out.extend(open.iter().rev());
    out
}

/// Emacs editing, plus: completion opens by itself as you type a name, or
/// right after `$`, `@` and `::` (the completer decides whether there is
/// anything worth showing). Tab presses are noted for the completer, which
/// offers suggestions for shorter prefixes when asked explicitly.
pub struct AutoCompleteMode<M: EditMode> {
    inner: M,
    tab_pressed: Arc<AtomicBool>,
    /// In Ctrl-R history search, which ignores `Multiple` events.
    searching: bool,
}

impl<M: EditMode> AutoCompleteMode<M> {
    pub fn new(inner: M, tab_pressed: Arc<AtomicBool>) -> Self {
        Self { inner, tab_pressed, searching: false }
    }
}

impl<M: EditMode> EditMode for AutoCompleteMode<M> {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw: Event = event.into();
        let is_tab = matches!(raw, Event::Key(KeyEvent { code: KeyCode::Tab, .. }));
        self.tab_pressed.store(is_tab, Ordering::SeqCst);
        // Converting back can't fail: reedline already accepted this event.
        let Ok(event) = ReedlineRawEvent::try_from(raw) else {
            return ReedlineEvent::None;
        };
        let parsed = self.inner.parse_event(event);
        if self.searching {
            // Typed letters must reach the search term unwrapped, and Enter
            // (bound as Esc + Enter, to close a menu first) must take the match.
            let parsed = match parsed {
                ReedlineEvent::Multiple(events) if events.contains(&ReedlineEvent::Enter) => ReedlineEvent::Enter,
                other => other,
            };
            self.searching = !matches!(
                parsed,
                ReedlineEvent::Enter
                    | ReedlineEvent::Submit
                    | ReedlineEvent::SubmitOrNewline
                    | ReedlineEvent::HistoryHintComplete
                    | ReedlineEvent::Esc
                    | ReedlineEvent::CtrlC
                    | ReedlineEvent::CtrlD
                    | ReedlineEvent::ExecuteHostCommand(_)
            );
            return parsed;
        }
        self.searching = parsed == ReedlineEvent::SearchHistory;
        match &parsed {
            ReedlineEvent::Edit(commands)
                if matches!(commands.as_slice(), [EditCommand::InsertChar(c)] if is_ident(*c) || matches!(c, '$' | '@' | ':')) =>
            {
                ReedlineEvent::Multiple(vec![parsed, ReedlineEvent::Menu(COMPLETION_MENU.into())])
            },
            _ => parsed,
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }

    fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
        self.inner.handle_mode_specific_event(event)
    }
}

/// The completion items behind the menu's suggestions (recorded by the
/// completer) and their help (fetched by the menu as they're selected).
#[derive(Default)]
pub struct Docs {
    /// The LSP item behind each suggestion the completer last returned, in order.
    pub items: Vec<Value>,
    /// The help for each item fetched so far, by [`doc_key`].
    pub help: HashMap<String, Help>,
}

pub type SharedDocs = Arc<Mutex<Docs>>;

/// Identifies a completion item across queries: `mean` from base is the same
/// item whatever prefix found it.
pub fn doc_key(item: &Value) -> String {
    format!("{}\u{0}{}\u{0}{}", item["label"], item["labelDetails"]["description"], item["data"])
}

/// How long fetching help for the selected suggestion may hold up drawing it.
const RESOLVE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Selection moves the menu has been sent but not yet applied.
#[derive(Clone, Copy)]
enum Move {
    Reset,
    Next,
    Previous,
    /// A page move, whose size only the menu knows.
    Unknown,
}

/// The completion menu, adjusted for completion as you type:
///
/// - It closes when there's nothing to offer, instead of showing "NO RECORDS
///   FOUND": most keystrokes have nothing worth showing.
/// - It never inserts a lone suggestion by itself. Reedline's quick
///   completions are on so that typing refreshes an open menu at once (Tab
///   right after a burst of typing then takes a current suggestion), but a
///   menu that opens with one suggestion would also accept it: typing `mea`
///   would become `mean(`. So the quick-completion check is refused once, as
///   the menu opens, and the menu loads its values when it's next drawn.
/// - Beside the list is the selected suggestion's help: from its help page
///   the function's usage, the title, description and arguments, with line
///   breaks and colours; or its signature if it has no help page. ark sends
///   help only for an item asked about (`completionItem/resolve`), so it's
///   fetched as a suggestion is selected, then kept. Reedline's own
///   description box can't show it (it re-wraps text on whitespace and
///   counts escape codes as text), so this draws the box itself.
pub struct CompletionMenu<M: Menu> {
    inner: M,
    opening: Cell<bool>,
    /// Shared with the completer: counts the times the menu opened (odd
    /// while open), so it can tell one opening from the next.
    openings: Arc<AtomicU64>,
    docs: SharedDocs,
    backend: CompletionBackend,
    /// The selected suggestion, mirrored from moves the menu applies (it
    /// doesn't say), and the last move sent but not yet applied.
    selected: Option<usize>,
    pending: Option<Move>,
    /// The help box as laid out for the next draw.
    help_box: Option<HelpBox>,
}

/// The selected suggestion's help, wrapped to fit beside the list.
struct HelpBox {
    lines: Vec<Vec<Span>>,
    side: Side,
}

/// Widest the help box gets, border included.
const HELP_MAX_WIDTH: usize = 76;
/// Narrower than this, there's no room for a useful help box.
const HELP_MIN_WIDTH: usize = 28;
/// Tallest the help box gets, border included.
const HELP_MAX_ROWS: usize = 16;

impl<M: Menu> CompletionMenu<M> {
    pub fn new(inner: M, openings: Arc<AtomicU64>, docs: SharedDocs, backend: CompletionBackend) -> Self {
        Self {
            inner,
            opening: Cell::new(false),
            openings,
            docs,
            backend,
            selected: Some(0),
            pending: None,
            help_box: None,
        }
    }

    /// Keeps `openings` odd exactly while the menu is open.
    fn track(&self) {
        let n = self.openings.load(Ordering::SeqCst);
        if (n % 2 == 1) != self.inner.is_active() {
            self.openings.store(n + 1, Ordering::SeqCst);
        }
    }

    fn close_if_empty(&mut self) {
        if self.inner.is_active() && self.inner.get_values().is_empty() && !self.inner.results_are_provisional() {
            self.inner.menu_event(MenuEvent::Deactivate);
        }
        self.track();
    }

    fn empty(&self) -> bool {
        self.inner.get_values().is_empty()
    }

    /// Mirrors the menu applying the pending move (as `IdeMenu` does: one
    /// move per draw, wrapping at the ends).
    fn apply_pending(&mut self) {
        let n = self.inner.get_values().len();
        self.selected = match (self.pending.take(), self.selected) {
            (None, selected) => selected,
            (Some(Move::Reset), _) => Some(0),
            (Some(Move::Next), Some(i)) => Some(if i + 1 < n { i + 1 } else { 0 }),
            (Some(Move::Previous), Some(i)) => Some(if i > 0 { i - 1 } else { n.saturating_sub(1) }),
            _ => None,
        };
    }

    /// Fetches help for the selected suggestion if it has none yet.
    fn fetch_selected_help(&mut self, editor: &Editor) {
        let Some(selected) = self.selected.filter(|&i| i < self.inner.get_values().len()) else {
            return;
        };
        let item = {
            let docs = self.docs.lock().unwrap();
            match docs.items.get(selected) {
                Some(item) if !docs.help.contains_key(&doc_key(item)) => item.clone(),
                _ => return,
            }
        };
        let namespace = namespace_before(editor.get_buffer(), self.inner.get_values()[selected].span.start);
        let help = self.help_for(&item, namespace.as_deref());
        // Kept even when empty, so nothing is asked twice.
        self.docs.lock().unwrap().help.insert(doc_key(&item), help);
    }

    /// The help for `item`: from its help page if it has one, with its
    /// signature in place of a usage it doesn't list (functions only).
    fn help_for(&self, item: &Value, namespace: Option<&str>) -> Help {
        let label = item["label"].as_str().unwrap_or_default();
        let resolved = self.backend.resolve(item, RESOLVE_TIMEOUT).ok();
        let markdown = resolved.as_ref().and_then(|r| {
            r["documentation"]["value"].as_str().or_else(|| r["documentation"].as_str()).map(str::to_string)
        });
        let help = markdown.as_deref().map(|md| help_from_markdown(label, md)).unwrap_or_default();
        let is_function = matches!(item["kind"].as_u64(), Some(2 | 3));
        if !is_function || help.has_usage {
            return help;
        }
        // No usage on a help page (a function defined in the session, an S3
        // method listed under its generic): ask for its signature instead.
        let call = format!("{}{label}()", namespace.map(|ns| format!("{ns}::")).unwrap_or_default());
        let signature = self
            .backend
            .signature(&call, call.len() - 1, RESOLVE_TIMEOUT)
            .ok()
            .and_then(|h| h["signatures"][0]["label"].as_str().map(str::to_string))
            // ark labels the signature with the call as written: drop the `pkg::`.
            .map(|label| {
                label
                    .split_once("::")
                    .filter(|(ns, _)| !ns.contains('('))
                    .map_or(label.clone(), |(_, rest)| rest.trim_start_matches(':').to_string())
            });
        help.with_usage(signature.as_slice())
    }

    /// Lays out the selected suggestion's help beside the list: on the right
    /// if it fits there, otherwise in the room left of the list.
    fn lay_out_help(&mut self, painter: &Painter) {
        self.help_box = None;
        let Some(selected) = self.selected.filter(|&i| i < self.inner.get_values().len()) else {
            return;
        };
        let help = {
            let docs = self.docs.lock().unwrap();
            let Some(item) = docs.items.get(selected) else { return };
            match docs.help.get(&doc_key(item)) {
                Some(help) if !help.is_empty() => help.clone(),
                // Until its help arrives, or if it has none: what it is.
                _ => match kind_label(item) {
                    Some(kind) => Help::note(&kind),
                    None => return,
                },
            }
        };
        let list = self.inner.menu_string(u16::MAX, false);
        let rows: Vec<&str> = list.split("\r\n").collect();
        let right_edge = rows.iter().map(|r| visible_width(r.trim_end())).max().unwrap_or(0);
        let indent = rows.iter().map(|r| r.len() - r.trim_start_matches(' ').len()).min().unwrap_or(0);
        let screen = painter.screen_width() as usize;
        let room_right = screen.saturating_sub(right_edge + 1);
        let room_left = indent.saturating_sub(1);
        let (side, room) = if room_right >= HELP_MIN_WIDTH || room_right >= room_left {
            (Side::Right, room_right)
        } else {
            (Side::Left, room_left)
        };
        let width = room.min(HELP_MAX_WIDTH);
        if width < HELP_MIN_WIDTH.min(16) + BOX_FRAME {
            return;
        }
        let lines = wrap(&help, width - BOX_FRAME);
        if !lines.is_empty() {
            self.help_box = Some(HelpBox { lines, side });
        }
    }

    fn help_rows(&self) -> usize {
        self.help_box.as_ref().map_or(0, |b| (b.lines.len() + 2).min(HELP_MAX_ROWS))
    }
}

/// The package a name is being completed in, from `pkg::` or `pkg:::` before it.
fn namespace_before(text: &str, start: usize) -> Option<String> {
    let before = text.get(..start)?.strip_suffix("::")?;
    let before = before.strip_suffix(':').unwrap_or(before);
    let ns: String = before.chars().rev().take_while(|&c| is_ident(c)).collect::<Vec<_>>().into_iter().rev().collect();
    (!ns.is_empty()).then_some(ns)
}

impl<M: Menu> Menu for CompletionMenu<M> {
    fn settings(&self) -> &MenuSettings {
        self.inner.settings()
    }
    fn is_active(&self) -> bool {
        self.inner.is_active()
    }
    fn set_active(&mut self, active: bool) {
        self.inner.set_active(active)
    }
    fn clear_input(&mut self) {
        self.inner.clear_input()
    }
    fn on_activate(&mut self) {
        self.inner.on_activate()
    }
    fn on_deactivate(&mut self) {
        self.inner.on_deactivate()
    }
    fn menu_event(&mut self, event: MenuEvent) {
        let event = match event {
            MenuEvent::Activate(_) => {
                self.opening.set(true);
                // Not loaded yet (see above): the menu loads them when drawn.
                MenuEvent::Activate(false)
            },
            event => event,
        };
        // The menu keeps only the last move until it's drawn; so does this.
        self.pending = match event {
            MenuEvent::Activate(_) | MenuEvent::Edit(_) => Some(Move::Reset),
            MenuEvent::NextElement | MenuEvent::MoveDown => Some(Move::Next),
            MenuEvent::PreviousElement | MenuEvent::MoveUp => Some(Move::Previous),
            MenuEvent::NextPage | MenuEvent::PreviousPage => Some(Move::Unknown),
            MenuEvent::Deactivate | MenuEvent::MoveLeft | MenuEvent::MoveRight => self.pending,
        };
        self.inner.menu_event(event);
        self.track();
    }
    fn can_quick_complete(&self) -> bool {
        !self.opening.replace(false) && self.inner.can_quick_complete()
    }
    fn can_partially_complete(&mut self, values_updated: bool, editor: &mut Editor, completer: &mut dyn Completer) -> bool {
        self.inner.can_partially_complete(values_updated, editor, completer)
    }
    fn update_values(&mut self, editor: &mut Editor, completer: &mut dyn Completer) {
        self.inner.update_values(editor, completer);
        self.selected = Some(0);
        self.close_if_empty();
    }
    fn reset_position(&mut self) {
        self.inner.reset_position()
    }
    fn reload(&mut self, updated: bool, editor: &mut Editor, completer: &mut dyn Completer) {
        self.inner.reload(updated, editor, completer);
        self.selected = Some(0);
        self.close_if_empty();
    }
    fn update_working_details(&mut self, editor: &mut Editor, completer: &mut dyn Completer, painter: &Painter) {
        self.inner.update_working_details(editor, completer, painter);
        self.apply_pending();
        self.close_if_empty();
        if self.inner.is_active() {
            self.fetch_selected_help(editor);
            self.lay_out_help(painter);
        } else {
            self.help_box = None;
        }
    }
    fn replace_in_buffer(&self, editor: &mut Editor) {
        self.inner.replace_in_buffer(editor)
    }
    fn menu_required_lines(&self, terminal_columns: u16) -> u16 {
        if self.empty() {
            return 0;
        }
        self.inner.menu_required_lines(terminal_columns).max(self.help_rows() as u16)
    }
    fn menu_string(&self, available_lines: u16, use_ansi_coloring: bool) -> String {
        if self.empty() {
            return String::new();
        }
        let list = self.inner.menu_string(available_lines, use_ansi_coloring);
        match &self.help_box {
            Some(b) => {
                let rows = render_box(&b.lines, self.help_rows().min(available_lines as usize), use_ansi_coloring);
                beside(&list, &rows, b.side)
            },
            None => list,
        }
    }
    fn min_rows(&self) -> u16 {
        self.inner.min_rows()
    }
    fn get_values(&self) -> &[Suggestion] {
        self.inner.get_values()
    }
    fn results_are_provisional(&self) -> bool {
        self.inner.results_are_provisional()
    }
    fn is_awaiting_first_answer(&self) -> bool {
        self.inner.is_awaiting_first_answer()
    }
    fn is_visible(&self) -> bool {
        self.inner.is_visible() && !self.empty()
    }
    fn set_cursor_pos(&mut self, pos: (u16, u16)) {
        self.inner.set_cursor_pos(pos)
    }
    fn selection_at(&self, column: u16, menu_row: u16, terminal_columns: u16, available_lines: u16) -> Option<usize> {
        self.inner.selection_at(column, menu_row, terminal_columns, available_lines)
    }
    fn set_selection(&mut self, index: usize) {
        self.inner.set_selection(index);
        if index < self.inner.get_values().len() {
            self.selected = Some(index);
        }
    }
}

/// The signature of the call around the cursor, below the input, with the
/// current argument highlighted: `mean(x, trim = 0, na.rm = FALSE, ...)`.
pub struct SignatureLine {
    backend: CompletionBackend,
    /// What the last lookup was for, and ark's answer.
    cache: Option<(String, Option<Value>)>,
}

impl SignatureLine {
    pub fn new(backend: CompletionBackend) -> Self {
        Self { backend, cache: None }
    }
}

impl StatusLine for SignatureLine {
    fn status(&mut self, line: &str, pos: usize, columns: u16, use_ansi_coloring: bool) -> String {
        let Some(call) = code_context(line, pos).call else {
            return String::new();
        };
        // Ark's answer only changes with the call or the argument the cursor is in.
        let key = format!("{}\u{0}{}\u{0}{:?}", &line[..=call.open], call.arg_index, call.arg_name);
        if self.cache.as_ref().is_none_or(|(k, _)| *k != key) {
            // ark only finds a call that's closed, as it always is in an editor
            // that pairs brackets; at the prompt it's usually still open.
            let closed = close_brackets(line);
            let help = self.backend.signature(&closed, pos, SIGNATURE_TIMEOUT).ok().filter(|h| !h.is_null());
            self.cache = Some((key, help));
        }
        let Some(help) = self.cache.as_ref().and_then(|(_, h)| h.as_ref()) else {
            return String::new();
        };
        render_signature(help, columns as usize, use_ansi_coloring)
    }
}

/// One line: the signature, dimmed, with the active parameter in bold;
/// shortened around the active parameter to fit `columns`.
pub fn render_signature(help: &Value, columns: usize, color: bool) -> String {
    let index = help["activeSignature"].as_u64().unwrap_or(0) as usize;
    let Some(signature) = help["signatures"].get(index) else {
        return String::new();
    };
    let Some(label) = signature["label"].as_str() else {
        return String::new();
    };
    let active = signature["activeParameter"].as_u64().or_else(|| help["activeParameter"].as_u64());
    let range = active
        .and_then(|i| signature["parameters"].get(i as usize))
        .and_then(|p| parameter_range(label, &p["label"]));

    let chars: Vec<(usize, char)> = label.char_indices().collect();
    let width = columns.saturating_sub(1).max(10);
    let (mut first, mut last) = (0, chars.len());
    if chars.len() > width {
        // Keep the active parameter in view, leaving room for an ellipsis at each end.
        let window = width - 2;
        let focus = range.map_or(0, |(s, _)| chars.iter().position(|&(b, _)| b >= s).unwrap_or(0));
        first = focus.saturating_sub(window / 3).min(chars.len() - window);
        last = first + window;
    }
    let text = |from: usize, to: usize| -> String { chars[from..to].iter().map(|&(_, c)| c).collect() };
    let at = |byte: usize| chars.iter().position(|&(b, _)| b >= byte).unwrap_or(chars.len()).clamp(first, last);
    let (lead, trail) = (if first > 0 { "…" } else { "" }, if last < chars.len() { "…" } else { "" });
    let dim = Style::new().fg(Color::DarkGray);
    match range {
        Some((s, e)) if color => {
            let (s, e) = (at(s), at(e));
            format!(
                "{}{}{}",
                dim.paint(format!("{lead}{}", text(first, s))),
                Style::new().bold().paint(text(s, e)),
                dim.paint(format!("{}{trail}", text(e, last)))
            )
        },
        _ if color => dim.paint(format!("{lead}{}{trail}", text(first, last))).to_string(),
        _ => format!("{lead}{}{trail}", text(first, last)),
    }
}

/// A parameter's byte range in the signature label: ark gives offsets; other
/// servers may give the parameter's text.
fn parameter_range(label: &str, parameter: &Value) -> Option<(usize, usize)> {
    match parameter {
        Value::Array(offsets) => {
            let (s, e) = (offsets.first()?.as_u64()? as usize, offsets.get(1)?.as_u64()? as usize);
            (s <= e && e <= label.len() && label.is_char_boundary(s) && label.is_char_boundary(e)).then_some((s, e))
        },
        Value::String(text) => label.find(text.as_str()).map(|s| (s, s + text.len())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_the_call_and_argument_around_the_cursor() {
        let text = "x <- mean(y, na.rm = ";
        let call = code_context(text, text.len()).call.unwrap();
        assert_eq!((call.function.as_str(), call.arg_index, call.arg_name.as_deref()), ("mean", 1, Some("na.rm")));
        assert_eq!(call.open, 9);

        let text = "stats::sd(c(1, 2), ";
        let call = code_context(text, text.len()).call.unwrap();
        assert_eq!((call.function.as_str(), call.arg_index), ("stats::sd", 1));
        // Inside the inner call, it's `c`'s second argument.
        assert_eq!(code_context(text, 15).call.unwrap().function, "c");
    }

    #[test]
    fn strings_comments_and_keywords_are_not_calls() {
        assert!(code_context("paste(\"a, (b", 12).in_string_or_comment);
        assert!(code_context("f(x) # mean(", 12).in_string_or_comment);
        assert!(!code_context("`odd name`", 5).in_string_or_comment);
        assert_eq!(code_context("read.csv('data/", 15).open_quote, Some('\''));
        assert_eq!(code_context("`odd name", 9).open_quote, None);
        assert_eq!(code_context("x <- \"a\" # \"", 12).open_quote, None);
        assert_eq!(code_context("if (x", 5).call, None);
        assert_eq!(code_context("df[x, ", 6).call, None);
        assert_eq!(code_context("f(x == 1", 8).call.unwrap().arg_name, None);
        assert_eq!(code_context("f(\"(\", ", 7).call.unwrap().arg_index, 1);
    }

    #[test]
    fn finds_the_namespace_a_name_is_completed_in() {
        assert_eq!(namespace_before("x <- base::mea", 11).as_deref(), Some("base"));
        assert_eq!(namespace_before("stats:::sd", 8).as_deref(), Some("stats"));
        assert_eq!(namespace_before("mea", 0), None);
    }

    #[test]
    fn closes_what_the_input_leaves_open() {
        assert_eq!(close_brackets("mean(1:3, "), "mean(1:3, )");
        assert_eq!(close_brackets("f(x[1, g(\"a)"), "f(x[1, g(\"a)\")])");
        assert_eq!(close_brackets("f(x) # (oops"), "f(x) # (oops\n");
        assert_eq!(close_brackets("lapply(xs, function(x) {"), "lapply(xs, function(x) {})");
        assert_eq!(close_brackets("mean(x)"), "mean(x)");
    }

    #[test]
    fn signature_highlights_and_fits_the_active_parameter() {
        let help = json!({
            "signatures": [{
                "label": "mean(x, trim = 0, na.rm = FALSE)",
                "parameters": [{"label": [5, 6]}, {"label": [8, 16]}, {"label": [18, 31]}],
                "activeParameter": 2,
            }],
        });
        assert_eq!(render_signature(&help, 80, false), "mean(x, trim = 0, na.rm = FALSE)");
        let colored = render_signature(&help, 80, true);
        assert!(colored.contains(&Style::new().bold().paint("na.rm = FALSE").to_string()), "{colored:?}");
        // Too narrow: shortened, but the active parameter stays visible.
        let narrow = render_signature(&help, 20, false);
        assert!(narrow.starts_with('…') && narrow.contains("na.rm"), "{narrow:?}");
        assert!(narrow.chars().count() <= 20, "{narrow:?}");
    }
}
