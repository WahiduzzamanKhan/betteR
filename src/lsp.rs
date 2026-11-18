//! Console completion via ark's LSP: the input buffer is mirrored into a
//! virtual document and `textDocument/completion` is queried against it, so the
//! console and the editor share one completion engine and one live session.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reedline::{CompletionResult, Completer, Span, Suggestion};
use serde_json::{json, Value};
use unicode_width::UnicodeWidthStr;

use crate::assist::{code_context, SharedDocs};
use crate::ide::Ide;

const DOC_URI: &str = "untitled:better-console";
const TIMEOUT: Duration = Duration::from_secs(3);

pub struct LspClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    next_id: i64,
    version: i32,
    utf8_positions: bool,
}

impl LspClient {
    pub fn connect(port: u16, root: &std::path::Path) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port)).context("connecting to ark LSP")?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        let mut client = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            next_id: 1,
            version: 0,
            utf8_positions: false,
        };
        let root_uri = format!("file://{}", root.display());
        let init = client.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "general": { "positionEncodings": ["utf-8", "utf-16"] },
                    "textDocument": { "completion": { "completionItem": { "snippetSupport": true } } },
                },
            }),
        )?;
        client.utf8_positions = init["capabilities"]["positionEncoding"] == "utf-8";
        client.notify("initialized", json!({}))?;
        client.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": DOC_URI, "languageId": "r", "version": 0, "text": "" } }),
        )?;
        Ok(client)
    }

    fn write(&mut self, payload: &Value) -> Result<()> {
        let body = serde_json::to_vec(payload)?;
        write!(self.writer, "Content-Length: {}\r\n\r\n", body.len())?;
        self.writer.write_all(&body)?;
        Ok(self.writer.flush()?)
    }

    fn read(&mut self) -> Result<Value> {
        let mut length = None;
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("LSP connection closed");
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                length = Some(v.trim().parse::<usize>()?);
            }
        }
        let mut body = vec![0; length.ok_or_else(|| anyhow!("LSP message without Content-Length"))?];
        self.reader.read_exact(&mut body)?;
        Ok(serde_json::from_slice(&body)?)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let msg = self.read()?;
            let is_request = msg.get("method").is_some();
            match msg.get("id") {
                // Server-to-client request (e.g. registerCapability): ack and move on.
                Some(sid) if is_request => {
                    let sid = sid.clone();
                    self.write(&json!({ "jsonrpc": "2.0", "id": sid, "result": null }))?;
                },
                Some(rid) if rid == &json!(id) => {
                    if let Some(err) = msg.get("error") {
                        bail!("LSP {method} failed: {err}");
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                },
                // Notifications and stale responses to timed-out requests.
                _ => {},
            }
        }
    }

    /// Mirrors the input buffer into the console document.
    fn sync(&mut self, text: &str) -> Result<()> {
        self.version += 1;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": DOC_URI, "version": self.version },
                "contentChanges": [{ "text": text }],
            }),
        )
    }

    fn position(&self, text: &str, pos: usize) -> Value {
        let (line, character) = byte_to_position(text, pos, self.utf8_positions);
        json!({ "textDocument": { "uri": DOC_URI }, "position": { "line": line, "character": character } })
    }

    /// The LSP completion result for `text` with the cursor at byte `pos`.
    pub fn completions(&mut self, text: &str, pos: usize) -> Result<Value> {
        self.sync(text)?;
        let params = self.position(text, pos);
        self.request("textDocument/completion", params)
    }

    /// The completion item with its documentation filled in (`completionItem/resolve`).
    pub fn resolve(&mut self, item: &Value) -> Result<Value> {
        self.request("completionItem/resolve", item.clone())
    }

    /// The LSP signature help for the call around byte `pos`, or null.
    pub fn signature_help(&mut self, text: &str, pos: usize) -> Result<Value> {
        self.sync(text)?;
        let params = self.position(text, pos);
        self.request("textDocument/signatureHelp", params)
    }

    pub fn utf8_positions(&self) -> bool {
        self.utf8_positions
    }
}

fn byte_to_position(text: &str, pos: usize, utf8: bool) -> (u32, u32) {
    let before = &text[..pos];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = &before[line_start..];
    let character = if utf8 { col.len() } else { col.encode_utf16().count() };
    (line, character as u32)
}

fn position_to_byte(text: &str, pos: &Value, utf8: bool) -> Option<usize> {
    let line = pos["line"].as_u64()? as usize;
    let character = pos["character"].as_u64()? as usize;
    let mut offset = 0;
    for (i, l) in text.split('\n').enumerate() {
        if i == line {
            if utf8 {
                return Some(offset + character.min(l.len()));
            }
            let mut units = 0;
            for (b, ch) in l.char_indices() {
                if units >= character {
                    return Some(offset + b);
                }
                units += ch.len_utf16();
            }
            return Some(offset + l.len());
        }
        offset += l.len() + 1;
    }
    None
}

/// Start of the R identifier (or backticked name) ending at `pos`.
fn word_start(text: &str, pos: usize) -> usize {
    let before = &text[..pos];
    before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '.' || *c == '_')
        .last()
        .map_or(pos, |(i, _)| i)
}

/// Converts LSP completion items into reedline suggestions: resolves the span
/// to replace, filters by the typed prefix (LSP leaves filtering to clients),
/// and orders by the server's `sortText`.
#[cfg(test)]
pub fn to_suggestions(text: &str, pos: usize, items: &[Value], utf8: bool) -> Vec<Suggestion> {
    ranked_suggestions(text, pos, items, utf8).into_iter().map(|(s, _)| s).collect()
}

/// [`to_suggestions`], each with the item it came from.
fn ranked_suggestions<'a>(text: &str, pos: usize, items: &'a [Value], utf8: bool) -> Vec<(Suggestion, &'a Value)> {
    let mut out: Vec<(bool, String, Suggestion, &Value)> = Vec::new();
    for item in items {
        let label = item["label"].as_str().unwrap_or_default();
        let edit = &item["textEdit"];
        let range = edit.get("range").or_else(|| edit.get("replace"));
        let (start, end) = match range {
            Some(r) => match (position_to_byte(text, &r["start"], utf8), position_to_byte(text, &r["end"], utf8)) {
                (Some(s), Some(e)) if s <= pos && e >= s => (s, e.max(pos)),
                _ => (word_start(text, pos), pos),
            },
            None => (word_start(text, pos), pos),
        };
        let value = edit["newText"]
            .as_str()
            .or_else(|| item["insertText"].as_str())
            .unwrap_or(label)
            .to_string();
        let value = if item["insertTextFormat"] == 2 { snippet_to_cursor(&value) } else { value };

        let prefix = &text[start..pos];
        let filter = item["filterText"].as_str().unwrap_or(label);
        if !filter.to_lowercase().starts_with(&prefix.to_lowercase()) {
            continue;
        }
        let exact_case = filter.starts_with(prefix);
        let sort = item["sortText"].as_str().unwrap_or(label).to_string();

        out.push((
            !exact_case,
            sort,
            Suggestion {
                value,
                display_override: (label != filter || item["insertTextFormat"] == 2).then(|| label.to_string()),
                span: Span::new(start, end),
                ..Default::default()
            },
            item,
        ));
    }
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut seen = std::collections::HashSet::new();
    let mut ranked: Vec<(Suggestion, &Value)> = out
        .into_iter()
        .map(|(_, _, s, item)| (s, item))
        .filter(|(s, _)| seen.insert(s.value.clone()))
        .collect();
    add_package_column(&mut ranked);
    ranked
}

/// Widest name the package column lines up after; longer names push it out.
const NAME_COLUMN: usize = 40;

/// Shows each suggestion's package in a second column, as the editor does:
/// `mean        {base}`.
fn add_package_column(ranked: &mut [(Suggestion, &Value)]) {
    let package = |item: &Value| item["labelDetails"]["description"].as_str().filter(|p| !p.is_empty()).map(str::to_string);
    if !ranked.iter().any(|(_, item)| package(item).is_some()) {
        return;
    }
    let name = |s: &Suggestion| s.display_override.clone().unwrap_or_else(|| s.value.clone());
    let width = ranked.iter().map(|(s, _)| name(s).width()).max().unwrap_or(0).min(NAME_COLUMN);
    for (suggestion, item) in ranked.iter_mut() {
        if let Some(package) = package(item) {
            let name = name(suggestion);
            let pad = width.saturating_sub(name.width());
            suggestion.display_override = Some(format!("{name}{}  {package}", " ".repeat(pad)));
        }
    }
}

/// Reedline leaves the cursor after the inserted text, so keep only the text
/// up to the snippet's first tab stop: `fn($0)` -> `fn(`, as R's own completer does.
fn snippet_to_cursor(snippet: &str) -> String {
    let mut out = String::new();
    let mut chars = snippet.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            '$' if chars.peek().is_some_and(|c| *c == '{' || c.is_ascii_digit()) => break,
            _ => out.push(c),
        }
    }
    out
}

/// Where console completions come from: our own LSP connection to ark when
/// standalone, or the IDE's language client when hosted (so the kernel keeps a
/// single LSP client, which ark expects).
#[derive(Clone)]
pub enum CompletionBackend {
    Lsp(Arc<Mutex<LspClient>>),
    Ide(Arc<Ide>),
    None,
}

/// Completion items, whether the server said the list is incomplete (so it
/// mustn't be narrowed locally), and whether positions are UTF-8.
type Completions = (Vec<Value>, bool, bool);

impl CompletionBackend {
    pub fn completions(&self, text: &str, pos: usize, timeout: Duration) -> Result<Completions> {
        let (result, utf8) = match self {
            Self::Lsp(lsp) => {
                let mut lsp = lsp.lock().map_err(|_| anyhow!("LSP client unavailable"))?;
                (lsp.completions(text, pos)?, lsp.utf8_positions())
            },
            Self::Ide(ide) => (ide.request("complete", text, pos, timeout)?, false),
            Self::None => return Ok((vec![], false, false)),
        };
        let (items, incomplete) = match result {
            Value::Array(items) => (items, false),
            Value::Object(mut list) => match list.remove("items") {
                Some(Value::Array(items)) => (items, list.get("isIncomplete") == Some(&Value::Bool(true))),
                _ => (vec![], false),
            },
            _ => (vec![], false),
        };
        Ok((items, incomplete, utf8))
    }

    /// The completion item with its help filled in by ark.
    pub fn resolve(&self, item: &Value, timeout: Duration) -> Result<Value> {
        match self {
            Self::Lsp(lsp) => lsp.lock().map_err(|_| anyhow!("LSP client unavailable"))?.resolve(item),
            Self::Ide(ide) => ide.request_value(json!({ "type": "resolve", "item": item }), timeout),
            Self::None => Ok(item.clone()),
        }
    }

    /// ark's signature help for the call around byte `pos` (null if none).
    pub fn signature(&self, text: &str, pos: usize, timeout: Duration) -> Result<Value> {
        match self {
            Self::Lsp(lsp) => lsp.lock().map_err(|_| anyhow!("LSP client unavailable"))?.signature_help(text, pos),
            Self::Ide(ide) => ide.request("signature", text, pos, timeout),
            Self::None => Ok(Value::Null),
        }
    }
}

/// Typed completion pops up once a name has this many characters.
const AUTO_PREFIX: usize = 3;
/// Completion as you type gives up quickly rather than stall typing.
const AUTO_TIMEOUT: Duration = Duration::from_millis(800);
const EXPLICIT_TIMEOUT: Duration = Duration::from_secs(3);

/// The server's answer for one word, narrowed locally as the word grows.
struct Cached {
    /// The text before the word and after the cursor: the word's context.
    before: String,
    after: String,
    prefix: String,
    items: Vec<Value>,
    utf8: bool,
}

pub struct LspCompleter {
    backend: CompletionBackend,
    /// Help for the suggestions, shared with the menu that fetches it.
    docs: SharedDocs,
    /// Set (by the edit mode) when Tab asked for completions.
    tab_pressed: Arc<AtomicBool>,
    /// Counts the completion menu's openings, odd while open (kept by the menu).
    menu_openings: Arc<AtomicU64>,
    /// The word context Tab was pressed in, and the menu opening it led to:
    /// while that menu stays open there, any prefix gets suggestions.
    explicit: Option<(String, u64)>,
    cache: Option<Cached>,
}

impl LspCompleter {
    pub fn new(
        backend: CompletionBackend,
        docs: SharedDocs,
        tab_pressed: Arc<AtomicBool>,
        menu_openings: Arc<AtomicU64>,
    ) -> Self {
        Self { backend, docs, tab_pressed, menu_openings, explicit: None, cache: None }
    }

    /// The suggestions for `items`; notes which item each came from, for the
    /// menu to show its help.
    fn suggestions(&self, line: &str, pos: usize, items: &[Value], utf8: bool) -> Vec<Suggestion> {
        let ranked = ranked_suggestions(line, pos, items, utf8);
        self.docs.lock().unwrap().items = ranked.iter().map(|(_, item)| (*item).clone()).collect();
        ranked.into_iter().map(|(suggestion, _)| suggestion).collect()
    }

    /// Whether completion as you type has anything worth showing here: a name
    /// of at least three characters, or right after `$`, `@` or `::`; never
    /// inside strings or comments.
    fn wanted(line: &str, start: usize, pos: usize) -> bool {
        let before = &line[..start];
        let prefix = &line[start..pos];
        if prefix.starts_with(|c: char| c.is_ascii_digit()) || code_context(line, pos).in_string_or_comment {
            return false;
        }
        prefix.chars().count() >= AUTO_PREFIX || before.ends_with('$') || before.ends_with('@') || before.ends_with("::")
    }
}

impl Completer for LspCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let start = word_start(line, pos);
        let before = &line[..start];
        let after = &line[pos..];
        let prefix = &line[start..pos];
        let opening = self.menu_openings.load(Ordering::SeqCst);
        if self.tab_pressed.swap(false, Ordering::SeqCst) {
            self.explicit = Some((before.to_string(), opening));
        }
        let explicit = self.explicit.as_ref().is_some_and(|(b, o)| b == before && *o == opening);
        if !explicit && !Self::wanted(line, start, pos) {
            self.docs.lock().unwrap().items.clear();
            return CompletionResult::fresh(Vec::new());
        }

        let reusable = self
            .cache
            .as_ref()
            .is_some_and(|c| c.before == before && c.after == after && prefix.starts_with(c.prefix.as_str()));
        if !reusable {
            self.cache = None;
            let timeout = if explicit { EXPLICIT_TIMEOUT } else { AUTO_TIMEOUT };
            let Ok((items, incomplete, utf8)) = self.backend.completions(line, pos, timeout) else {
                self.docs.lock().unwrap().items.clear();
                return CompletionResult::fresh(Vec::new());
            };
            let suggestions = self.suggestions(line, pos, &items, utf8);
            // An incomplete list must be asked for again as the word grows.
            if !incomplete {
                self.cache = Some(Cached {
                    before: before.to_string(),
                    after: after.to_string(),
                    prefix: prefix.to_string(),
                    items,
                    utf8,
                });
            }
            return CompletionResult::fresh(suggestions);
        }
        let cached = self.cache.as_ref().expect("reusable cache");
        CompletionResult::fresh(self.suggestions(line, pos, &cached.items, cached.utf8))
    }
}

/// What kind of thing a completion is, shown beside the menu until its help
/// arrives, or instead of help (the package is in the list itself).
pub fn kind_label(item: &Value) -> Option<String> {
    let kind = match item["kind"].as_u64() {
        Some(2 | 3) => Some("function"),
        Some(5) => Some("column"),
        Some(6) => Some("variable"),
        Some(9) => Some("package"),
        Some(12) => Some("value"),
        Some(14) => Some("keyword"),
        Some(15) => Some("snippet"),
        Some(17) => Some("file"),
        Some(19) => Some("folder"),
        Some(21) => Some("constant"),
        Some(22) => Some("object"),
        _ => None,
    };
    let label = item["label"].as_str().unwrap_or_default();
    let detail = item["detail"].as_str().filter(|d| *d != label && !d.is_empty());
    detail.or(kind).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_roundtrip_with_utf16() {
        let text = "a <- \"é😀\"\nfoo$b";
        let pos = text.len();
        let (line, ch) = byte_to_position(text, pos, false);
        assert_eq!((line, ch), (1, 5));
        assert_eq!(position_to_byte(text, &json!({"line": 1, "character": 4}), false), Some(text.len() - 1));
        // "😀" is two UTF-16 units; position after it on line 0.
        let after_emoji = text.find('😀').unwrap() + '😀'.len_utf8();
        let (l, c) = byte_to_position(text, after_emoji, false);
        assert_eq!(position_to_byte(text, &json!({"line": l, "character": c}), false), Some(after_emoji));
    }

    #[test]
    fn suggestions_filter_prefix_and_use_text_edit_range() {
        let text = "y$c";
        let items = vec![
            json!({"label": "mpg", "sortText": "1"}),
            json!({"label": "cyl", "sortText": "2"}),
            json!({"label": "carb", "sortText": "3",
                   "textEdit": {"newText": "carb", "range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 3}}}}),
        ];
        let s = to_suggestions(text, 3, &items, false);
        let values: Vec<_> = s.iter().map(|s| (s.value.as_str(), s.span.start, s.span.end)).collect();
        assert_eq!(values, vec![("cyl", 2, 3), ("carb", 2, 3)]);
    }

    #[test]
    fn packages_are_a_second_column() {
        let items = vec![
            json!({"label": "mean", "sortText": "1", "insertText": "mean($0)", "insertTextFormat": 2, "labelDetails": {"description": "{base}"}}),
            json!({"label": "mean.Date", "sortText": "2", "labelDetails": {"description": "{base}"}}),
            json!({"label": "meaning", "sortText": "3"}),
        ];
        let shown: Vec<_> = to_suggestions("mea", 3, &items, false)
            .into_iter()
            .map(|s| (s.value, s.display_override))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("mean(".into(), Some("mean       {base}".into())),
                ("mean.Date".into(), Some("mean.Date  {base}".into())),
                ("meaning".into(), None),
            ]
        );
        // Without packages, names stand alone.
        assert_eq!(to_suggestions("y$c", 3, &[json!({"label": "cyl"})], false)[0].display_override, None);
    }

    #[test]
    fn dotted_identifiers_are_one_word() {
        assert_eq!(word_start("x <- is.na", 10), 5);
        assert_eq!(word_start("f(", 2), 2);
    }

    #[test]
    fn snippets_stop_at_first_tabstop() {
        assert_eq!(snippet_to_cursor("print_me_please($0)"), "print_me_please(");
        assert_eq!(snippet_to_cursor("lm(${1:formula})"), "lm(");
        assert_eq!(snippet_to_cursor("cost\\$"), "cost$");
        assert_eq!(snippet_to_cursor("mpg"), "mpg");
    }
}
