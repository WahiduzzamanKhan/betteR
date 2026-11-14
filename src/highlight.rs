//! Syntax highlighting for console input with tree-sitter-r's highlight query.

use std::sync::Mutex;

use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

pub struct RHighlighter {
    parser: Mutex<Parser>,
    query: Query,
    styles: Vec<Option<Style>>,
}

fn style_for(capture: &str) -> Option<Style> {
    let style = match capture {
        "comment" => Color::Fixed(244).italic(),
        "string" => Color::Green.normal(),
        "string.escape" => Color::Cyan.normal(),
        "number" | "boolean" | "constant.builtin" => Color::Purple.normal(),
        "keyword" | "keyword.function" | "conditional" | "repeat" => Color::Blue.bold(),
        "function" => Color::Yellow.normal(),
        "variable.parameter" => Color::Cyan.italic(),
        "namespace" => Color::LightCyan.normal(),
        "operator" => Color::LightRed.normal(),
        // Partial input is constantly "erroneous" while typing; don't paint it.
        _ => return None,
    };
    Some(style)
}

impl RHighlighter {
    pub fn new() -> Self {
        let language = tree_sitter_r::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).expect("tree-sitter-r grammar is compatible");
        let query = Query::new(&language, tree_sitter_r::HIGHLIGHTS_QUERY).expect("valid highlight query");
        let styles = query.capture_names().iter().map(|n| style_for(n)).collect();
        Self {
            parser: Mutex::new(parser),
            query,
            styles,
        }
    }

    /// Per-byte capture index. Innermost node wins; for the same node the
    /// later pattern wins (so `@function` overrides the generic `@variable`).
    fn classify(&self, line: &str) -> Vec<Option<u32>> {
        let mut out: Vec<Option<(usize, usize, u32)>> = vec![None; line.len()];
        let Some(tree) = self.parser.lock().unwrap().parse(line, None) else {
            return vec![None; line.len()];
        };
        let mut cursor = QueryCursor::new();
        let mut captures = cursor.captures(&self.query, tree.root_node(), line.as_bytes());
        while let Some((m, idx)) = captures.next() {
            let cap = m.captures()[*idx];
            let range = cap.node.byte_range();
            let len = range.len();
            for slot in &mut out[range] {
                let wins = match slot {
                    None => true,
                    Some((l, p, _)) => len < *l || (len == *l && m.pattern_index >= *p),
                };
                if wins {
                    *slot = Some((len, m.pattern_index, cap.index));
                }
            }
        }
        out.into_iter().map(|s| s.map(|(_, _, c)| c)).collect()
    }
}

impl Highlighter for RHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut text = StyledText::new();
        if line.is_empty() {
            return text;
        }
        let classes = self.classify(line);
        let mut start = 0;
        for (i, _) in line.char_indices().skip(1).chain(std::iter::once((line.len(), ' '))) {
            if i < line.len() && classes[i] == classes[start] {
                continue;
            }
            let style = classes[start].and_then(|c| self.styles[c as usize]).unwrap_or_default();
            text.push((style, line[start..i].to_string()));
            start = i;
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(line: &str) -> Vec<(Style, String)> {
        RHighlighter::new().highlight(line, 0).buffer
    }

    fn style_of(line: &str, token: &str) -> Style {
        styled(line).into_iter().find(|(_, t)| t == token).unwrap_or_else(|| panic!("no segment {token:?}")).0
    }

    #[test]
    fn highlights_core_tokens() {
        let line = r#"f <- function(x) if (TRUE) print("hi\n") # note"#;
        assert_eq!(styled(line).iter().map(|(_, t)| t.as_str()).collect::<String>(), line);
        assert_eq!(style_of(line, "function"), Color::Blue.bold());
        assert_eq!(style_of(line, "TRUE"), Color::Purple.normal());
        assert_eq!(style_of(line, "print"), Color::Yellow.normal());
        assert_eq!(style_of(line, "# note"), Color::Fixed(244).italic());
        assert_eq!(style_of(line, "\\n"), Color::Cyan.normal());
    }

    #[test]
    fn handles_partial_and_multibyte_input() {
        for line in ["", "mean(", "x <- \"é😀", "for (i in 1:3) {\n  print(i)"] {
            let text: String = styled(line).into_iter().map(|(_, t)| t).collect();
            assert_eq!(text, line);
        }
    }
}
