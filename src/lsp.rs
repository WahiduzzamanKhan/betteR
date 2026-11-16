//! Console completion via ark's LSP: the input buffer is mirrored into a
//! virtual document and `textDocument/completion` is queried against it, so the
//! console and the editor share one completion engine and one live session.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reedline::{CompletionResult, Completer, Span, Suggestion};
use serde_json::{json, Value};

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

    /// Returns raw LSP completion items for `text` with the cursor at byte `pos`.
    pub fn completions(&mut self, text: &str, pos: usize) -> Result<Vec<Value>> {
        self.version += 1;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": DOC_URI, "version": self.version },
                "contentChanges": [{ "text": text }],
            }),
        )?;
        let (line, character) = byte_to_position(text, pos, self.utf8_positions);
        let result = self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": DOC_URI },
                "position": { "line": line, "character": character },
            }),
        )?;
        Ok(match result {
            Value::Array(items) => items,
            Value::Object(mut list) => match list.remove("items") {
                Some(Value::Array(items)) => items,
                _ => vec![],
            },
            _ => vec![],
        })
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
pub fn to_suggestions(text: &str, pos: usize, items: &[Value], utf8: bool) -> Vec<Suggestion> {
    let mut out: Vec<(bool, String, Suggestion)> = Vec::new();
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
                // Descriptions would force a one-column menu; R users expect compact columns.
                display_override: (label != filter || item["insertTextFormat"] == 2).then(|| label.to_string()),
                span: Span::new(start, end),
                ..Default::default()
            },
        ));
    }
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut seen = std::collections::HashSet::new();
    out.into_iter()
        .map(|(_, _, s)| s)
        .filter(|s| seen.insert(s.value.clone()))
        .collect()
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
pub enum CompletionBackend {
    Lsp(Arc<Mutex<LspClient>>),
    Ide(Arc<Ide>),
    None,
}

pub struct LspCompleter {
    pub backend: CompletionBackend,
}

impl Completer for LspCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let suggestions = match &self.backend {
            CompletionBackend::Lsp(lsp) => match lsp.lock() {
                Ok(mut lsp) => match lsp.completions(line, pos) {
                    Ok(items) => to_suggestions(line, pos, &items, lsp.utf8_positions()),
                    Err(_) => Vec::new(),
                },
                Err(_) => Vec::new(),
            },
            CompletionBackend::Ide(ide) => match ide.complete(line, pos) {
                Ok(items) => to_suggestions(line, pos, &items, false),
                Err(_) => Vec::new(),
            },
            CompletionBackend::None => Vec::new(),
        };
        CompletionResult::fresh(suggestions)
    }
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
