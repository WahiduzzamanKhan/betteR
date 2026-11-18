//! The help box beside the completion menu: a suggestion's help page, laid
//! out with its own line breaks and colours. Reedline's menus draw a
//! description as one plain paragraph (they re-wrap it on whitespace and
//! measure escape codes as text), so the menu draws its list and this draws
//! the box next to it.

use nu_ansi_term::{Color, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A run of text in one style.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub style: Style,
    pub text: String,
}

impl Span {
    fn new(style: Style, text: impl Into<String>) -> Self {
        Self { style, text: text.into() }
    }
}

/// Help laid out as paragraphs of styled text; an empty paragraph is a blank line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Help {
    pub paragraphs: Vec<Vec<Span>>,
    /// Whether it starts with the function's usage.
    pub has_usage: bool,
}

impl Help {
    pub fn is_empty(&self) -> bool {
        self.paragraphs.iter().all(|p| p.is_empty())
    }

    /// Code (a usage, a signature) as its own paragraphs, ahead of the rest.
    pub fn with_usage(mut self, usage: &[String]) -> Self {
        let mut paragraphs: Vec<Vec<Span>> = usage.iter().map(|u| vec![Span::new(code(), u.clone())]).collect();
        if !self.is_empty() && !paragraphs.is_empty() {
            paragraphs.push(Vec::new());
        }
        paragraphs.append(&mut self.paragraphs);
        Self { paragraphs, has_usage: self.has_usage || !usage.is_empty() }
    }

    /// One line of dimmed text, e.g. what kind of thing a suggestion is.
    pub fn note(text: &str) -> Self {
        Self { paragraphs: vec![vec![Span::new(Style::new().dimmed(), text)]], has_usage: false }
    }
}

fn code() -> Style {
    Style::new().fg(Color::Cyan)
}

fn heading() -> Style {
    Style::new().bold()
}

/// A help page as ark renders it in Markdown, boiled down to what's useful
/// beside a suggestion: the usage of `name` (a page may document several
/// functions), the title, the description and the arguments.
pub fn help_from_markdown(name: &str, markdown: &str) -> Help {
    let mut title = String::new();
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    let mut in_code = false;
    for line in markdown.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
        }
        // In code, `## Default S3 method:` is an R comment, not a heading.
        let heading = |prefix: &str| if in_code { None } else { line.strip_prefix(prefix) };
        if let Some(t) = heading("## ").filter(|_| title.is_empty()) {
            title = t.trim().to_string();
        } else if let Some(h) = heading("### ") {
            sections.push((h.trim().to_string(), Vec::new()));
        } else if let Some((_, lines)) = sections.last_mut() {
            lines.push(line);
        }
    }
    let section = |name: &str| sections.iter().find(|(h, _)| h == name).map(|(_, l)| l.as_slice()).unwrap_or(&[]);

    let mut help = Help::default();
    if !title.is_empty() {
        help.paragraphs.push(vec![Span::new(heading(), inline_plain(&title))]);
    }
    for paragraph in paragraphs(section("Description")) {
        help.paragraphs.push(Vec::new());
        help.paragraphs.push(inline(&paragraph));
    }
    let arguments: Vec<Vec<Span>> = section("Arguments").iter().filter_map(|row| argument(row)).collect();
    if !arguments.is_empty() {
        help.paragraphs.push(Vec::new());
        help.paragraphs.push(vec![Span::new(heading(), "Arguments")]);
        help.paragraphs.extend(arguments);
    }
    help.with_usage(&usage_of(name, section("Usage")))
}

/// The usage entries for `name` in a Usage section, each whole across its
/// continuation lines.
fn usage_of(name: &str, lines: &[&str]) -> Vec<String> {
    let mut usages = Vec::new();
    let mut current: Option<(String, i32)> = None;
    for line in lines.iter().filter(|l| !l.starts_with("```")) {
        if current.is_none() && line.trim_start().starts_with(&format!("{name}(")) {
            current = Some((String::new(), 0));
        }
        if let Some((text, depth)) = current.as_mut() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(line.trim());
            *depth += line.matches('(').count() as i32 - line.matches(')').count() as i32;
            if *depth <= 0 {
                usages.push(text.split_whitespace().collect::<Vec<_>>().join(" "));
                current = None;
            }
        }
    }
    usages
}

/// Lines grouped into paragraphs at blank lines.
fn paragraphs(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in lines {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(' ');
            current.push_str(line.trim());
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// A row of the Arguments table, `| `x` | An R object. |`: `x — An R object.`, name in bold.
fn argument(row: &str) -> Option<Vec<Span>> {
    let cells: Vec<&str> = row.trim().strip_prefix('|')?.strip_suffix('|')?.split('|').map(str::trim).collect();
    let [name, text] = cells.as_slice() else { return None };
    let name = inline_plain(name);
    if name.is_empty() || name.chars().all(|c| c == '-') {
        return None;
    }
    let mut spans = vec![Span::new(Style::new().bold(), name), Span::new(Style::new().dimmed(), " — ")];
    spans.extend(inline(text));
    Some(spans)
}

/// Inline Markdown as styled text: `code` in colour, **bold**, links as their text.
fn inline(text: &str) -> Vec<Span> {
    let text = links_as_text(text);
    let mut spans: Vec<Span> = Vec::new();
    for (i, part) in text.split('`').enumerate() {
        if i % 2 == 1 {
            spans.push(Span::new(code(), unescape(part)));
            continue;
        }
        for (j, piece) in part.split("**").enumerate() {
            let style = if j % 2 == 1 { Style::new().bold() } else { Style::new() };
            if !piece.is_empty() {
                spans.push(Span::new(style, unescape(piece)));
            }
        }
    }
    spans.retain(|s| !s.text.is_empty());
    spans
}

fn inline_plain(text: &str) -> String {
    inline(text).into_iter().map(|s| s.text).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unescape(text: &str) -> String {
    text.replace("\\_", "_").replace("\\*", "*")
}

fn links_as_text(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        match rest[open..].find("](").and_then(|m| rest[open + m..].find(')').map(|c| (m, open + m + c))) {
            Some((mid, close)) => {
                out.push_str(&rest[..open]);
                out.push_str(&rest[open + 1..open + mid]);
                rest = &rest[close + 1..];
            },
            None => {
                out.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            },
        }
    }
    out.push_str(rest);
    out
}

/// Word-wraps help into lines at most `width` columns wide. Words longer than
/// a line are broken; blank lines at the ends and doubled ones are dropped.
pub fn wrap(help: &Help, width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span>> = Vec::new();
    for paragraph in &help.paragraphs {
        if paragraph.is_empty() {
            if lines.last().is_some_and(|l| !l.is_empty()) {
                lines.push(Vec::new());
            }
            continue;
        }
        let mut line: Vec<Span> = Vec::new();
        let mut used = 0;
        // Whitespace between spans counts, so words are taken with what precedes them.
        let mut gap = false;
        for span in paragraph {
            let starts_with_space = span.text.starts_with(char::is_whitespace);
            for (k, word) in span.text.split_whitespace().enumerate() {
                let space = used > 0 && (k > 0 || gap || starts_with_space);
                let w = word.width();
                if used > 0 && used + space as usize + w > width {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                if used > 0 && space {
                    push(&mut line, span.style, " ");
                    used += 1;
                }
                // A word wider than the line is broken over several.
                let mut piece = String::new();
                for c in word.chars() {
                    let cw = c.width().unwrap_or(0);
                    if used + cw > width && used > 0 {
                        push(&mut line, span.style, &std::mem::take(&mut piece));
                        lines.push(std::mem::take(&mut line));
                        used = 0;
                    }
                    piece.push(c);
                    used += cw;
                }
                push(&mut line, span.style, &piece);
            }
            gap = span.text.ends_with(char::is_whitespace);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Appends text to a line, merging it into the last span if the style matches.
fn push(line: &mut Vec<Span>, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }
    match line.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => line.push(Span::new(style, text)),
    }
}

fn line_width(line: &[Span]) -> usize {
    line.iter().map(|s| s.text.width()).sum()
}

/// The box's rows: a border round `lines` (each padded to the widest), cut to
/// `max_rows` with "…" when the help doesn't fit.
pub fn render_box(lines: &[Vec<Span>], max_rows: usize, color: bool) -> Vec<String> {
    if lines.is_empty() || max_rows < 3 {
        return Vec::new();
    }
    let inner = lines.iter().map(|l| line_width(l)).max().unwrap_or(0).max(1);
    let fits = max_rows - 2;
    let mut shown: Vec<Vec<Span>> = lines.iter().take(fits).cloned().collect();
    if lines.len() > fits {
        let last = shown.last_mut().expect("at least one row");
        // Make room for the ellipsis at the end of the last line shown.
        while line_width(last) + 1 > inner {
            let span = last.last_mut().expect("non-empty line");
            span.text.pop();
            if span.text.is_empty() {
                last.pop();
            }
        }
        push(last, Style::new().dimmed(), "…");
    }
    let border = |s: String| if color { Style::new().fg(Color::DarkGray).paint(s).to_string() } else { s };
    let mut rows = vec![border(format!("╭{}╮", "─".repeat(inner + 2)))];
    for line in &shown {
        let mut row = border("│ ".into());
        for span in line {
            if color && span.style != Style::new() {
                row.push_str(&span.style.paint(&span.text).to_string());
            } else {
                row.push_str(&span.text);
            }
        }
        row.push_str(&" ".repeat(inner - line_width(line)));
        row.push_str(&border(" │".into()));
        rows.push(row);
    }
    rows.push(border(format!("╰{}╯", "─".repeat(inner + 2))));
    rows
}

/// Columns a box adds round its text: border and one space each side.
pub const BOX_FRAME: usize = 4;

/// The display width of terminal text, skipping escape sequences.
pub fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // CSI: parameters, then a final byte in @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        width += c.width().unwrap_or(0);
    }
    width
}

/// `text` without trailing spaces, even ones followed by escape codes (a
/// coloured row often ends in a reset); the codes are kept.
fn trim_visible_end(text: &str) -> String {
    let mut codes = String::new();
    let mut rest = text;
    loop {
        if let Some(stripped) = rest.strip_suffix(' ') {
            rest = stripped;
        } else if let Some(start) = rest.rfind("\x1b[").filter(|&i| {
            let tail = &rest[i + 2..];
            !tail.is_empty() && tail.is_ascii() && tail[..tail.len() - 1].chars().all(|c| c.is_ascii_digit() || c == ';') && tail.ends_with(|c: char| ('@'..='~').contains(&c))
        }) {
            codes.insert_str(0, &rest[start..]);
            rest = &rest[..start];
        } else {
            break;
        }
    }
    format!("{rest}{codes}")
}

/// Where the help box goes relative to the menu's list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Side {
    Right,
    Left,
}

/// Joins the menu's list rows (`\r\n`-separated, each indented to line up
/// with the word being completed) with the help box's rows, side by side.
pub fn beside(list: &str, help: &[String], side: Side) -> String {
    let rows: Vec<String> = if list.is_empty() { Vec::new() } else { list.split("\r\n").map(trim_visible_end).collect() };
    let right_edge = rows.iter().map(|r| visible_width(r)).max().unwrap_or(0);
    let n = rows.len().max(help.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let row = &rows.get(i).cloned().unwrap_or_default();
        let help_row = help.get(i).map(String::as_str).unwrap_or("");
        out.push(match side {
            Side::Right if help_row.is_empty() => row.clone(),
            Side::Right => format!("{row}{} {help_row}", " ".repeat(right_edge.saturating_sub(visible_width(row)))),
            Side::Left => {
                // The box takes the place of the list's indentation.
                let skip = help.first().map_or(0, |h| visible_width(h)) + 1;
                let rest = if row.len() >= skip && row[..skip].chars().all(|c| c == ' ') { &row[skip..] } else { row.as_str() };
                let pad = if help_row.is_empty() { " ".repeat(skip) } else { format!("{help_row} ") };
                format!("{pad}{rest}")
            },
        });
    }
    out.join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const MEAN_HELP: &str = "_mean {base}_\n\n## Arithmetic Mean\n\n------\n### Description\n\n\nGeneric function for the (trimmed) arithmetic\nmean.\n\n### Usage\n\n``` r\nmean(x, ...)\n\n## Default S3 method:\nmean(x, trim = 0, na.rm = FALSE, ...)\n```\n\n### Arguments\n\n|     |     |\n| --- | --- |\n| `x` | An **R** object. See [mean](../mean.html). |\n| `trim` | the fraction (0 to 0.5) of observations to be trimmed. |\n";

    fn text(lines: &[Vec<Span>]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect()
    }

    #[test]
    fn help_keeps_usage_title_description_and_arguments_apart() {
        let help = help_from_markdown("mean", MEAN_HELP);
        assert!(help.has_usage);
        assert_eq!(
            text(&wrap(&help, 80)),
            vec![
                "mean(x, ...)",
                "mean(x, trim = 0, na.rm = FALSE, ...)",
                "",
                "Arithmetic Mean",
                "",
                "Generic function for the (trimmed) arithmetic mean.",
                "",
                "Arguments",
                "x — An R object. See mean.",
                "trim — the fraction (0 to 0.5) of observations to be trimmed.",
            ]
        );
        // Usage is code; the title and argument names are bold.
        assert_eq!(help.paragraphs[0][0].style, code());
        assert_eq!(help.paragraphs[3][0].style, heading());
        let x = help.paragraphs.iter().find(|p| p.first().is_some_and(|s| s.text == "x")).unwrap();
        assert_eq!(x[0].style, Style::new().bold());
        assert!(x.iter().any(|s| s.text == "R" && s.style == Style::new().bold()));
    }

    #[test]
    fn usage_only_for_the_function_asked_about() {
        let md = "## Complex Numbers\n### Usage\n``` r\ncomplex(length.out = 0, real = numeric(),\n        modulus = 1)\nArg(z)\n```\n";
        assert_eq!(text(&wrap(&help_from_markdown("complex", md), 80))[0], "complex(length.out = 0, real = numeric(), modulus = 1)");
        assert_eq!(text(&wrap(&help_from_markdown("Arg", md), 80))[0], "Arg(z)");
        assert!(!help_from_markdown("stats", "## The R Stats Package\n").has_usage);
    }

    #[test]
    fn wraps_words_keeping_styles_and_breaking_long_ones() {
        let help = Help {
            paragraphs: vec![vec![
                Span::new(Style::new(), "the value of "),
                Span::new(code(), "na.rm"),
                Span::new(Style::new(), " decides"),
            ]],
            has_usage: false,
        };
        let lines = wrap(&help, 14);
        assert_eq!(text(&lines), vec!["the value of", "na.rm decides"]);
        assert_eq!(lines[1][0], Span::new(code(), "na.rm"));
        assert_eq!(text(&wrap(&Help::note("abcdefghij"), 4)), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn box_fits_its_rows_and_marks_what_is_cut() {
        let lines = wrap(&help_from_markdown("mean", MEAN_HELP), 40);
        let rows = render_box(&lines, 6, false);
        assert_eq!(rows.len(), 6);
        assert!(rows[0].starts_with('╭') && rows[5].starts_with('╰'));
        assert!(rows[4].trim_end_matches('│').trim_end().ends_with('…'), "{rows:?}");
        let widths: Vec<_> = rows.iter().map(|r| visible_width(r)).collect();
        assert!(widths.iter().all(|&w| w == widths[0]), "{widths:?}");
        // Colours don't change the layout.
        let colored = render_box(&lines, 6, true);
        assert_eq!(colored.iter().map(|r| visible_width(r)).collect::<Vec<_>>(), widths);
        assert!(colored[1].contains("\x1b["));
    }

    #[test]
    fn trims_spaces_hidden_before_escape_codes() {
        assert_eq!(trim_visible_end("│a│ \x1b[0m"), "│a│\x1b[0m");
        assert_eq!(trim_visible_end("ab  "), "ab");
        assert_eq!(trim_visible_end("ab\x1b[1m"), "ab\x1b[1m");
        // Text after an earlier code isn't mistaken for one (nor sliced mid-character).
        assert_eq!(trim_visible_end("\x1b[1mmean\x1b[0m│ "), "\x1b[1mmean\x1b[0m│");
    }

    #[test]
    fn box_goes_beside_the_list() {
        let list = "    ╭────╮\r\n    │mean│\r\n    ╰────╯";
        let help = vec!["╭─╮".to_string(), "│a│".into(), "│b│".into(), "╰─╯".into()];
        assert_eq!(
            beside(list, &help, Side::Right),
            "    ╭────╮ ╭─╮\r\n    │mean│ │a│\r\n    ╰────╯ │b│\r\n           ╰─╯"
        );
        assert_eq!(beside(list, &help, Side::Left), "╭─╮ ╭────╮\r\n│a│ │mean│\r\n│b│ ╰────╯\r\n╰─╯ ");
    }
}
