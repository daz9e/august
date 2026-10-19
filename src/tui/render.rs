//! Markdown -> styled terminal lines, and word wrapping that keeps styles.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

pub const ACCENT: Color = Color::Rgb(214, 132, 92);
pub const CODE: Color = Color::Rgb(130, 180, 220);
pub const DIM: Style = Style::new().fg(Color::DarkGray);

struct Renderer {
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// One entry per open list: the next number for ordered lists.
    lists: Vec<Option<u64>>,
    /// Indent for continuation lines of the current list item.
    hang: usize,
    quote: usize,
    code: bool,
    link: Option<(String, String)>,
    /// Rows of the table being read (cells as plain text); the first is the header.
    table: Option<Vec<Vec<String>>>,
    cell: String,
}

impl Renderer {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        self.styles.push(f(self.style()));
    }

    fn prefix(&self) -> Vec<Span<'static>> {
        let mut p = Vec::new();
        for _ in 0..self.quote {
            p.push(Span::styled("│ ", DIM));
        }
        p
    }

    /// Ends the current line (if it has content).
    fn flush(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let mut spans = self.prefix();
        if !self.lists.is_empty() && !matches!(self.cur.first(), Some(s) if s.content.starts_with('\u{0}')) {
            spans.push(Span::raw(" ".repeat(self.hang)));
        }
        for s in self.cur.drain(..) {
            // A list marker carries its own indentation (tagged with a leading NUL).
            match s.content.strip_prefix('\u{0}') {
                Some(rest) => spans.push(Span::styled(rest.to_string(), s.style)),
                None => spans.push(s),
            }
        }
        self.lines.push(Line::from(spans));
    }

    fn blank(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    /// Emits a table with aligned columns.
    fn table(&mut self, rows: Vec<Vec<String>>) {
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let width = |s: &str| unicode_width::UnicodeWidthStr::width(s);
        let widths: Vec<usize> = (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| width(s)).max().unwrap_or(0)).collect();
        for (i, row) in rows.iter().enumerate() {
            let mut spans = self.prefix();
            for (c, w) in widths.iter().enumerate() {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                let style = if i == 0 { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
                let pad = if c + 1 < cols { w - width(cell) + 2 } else { 0 };
                spans.push(Span::styled(format!("{cell}{}", " ".repeat(pad)), style));
            }
            self.lines.push(Line::from(spans));
            if i == 0 {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                let mut spans = self.prefix();
                spans.push(Span::styled(rule.join("  "), DIM));
                self.lines.push(Line::from(spans));
            }
        }
    }

    fn text(&mut self, t: &str) {
        if self.table.is_some() {
            self.cell.push_str(t);
            return;
        }
        if self.code {
            for (i, l) in t.split('\n').enumerate() {
                if i > 0 || !self.cur.is_empty() {
                    self.flush();
                }
                if !l.is_empty() || i == 0 {
                    let mut spans = self.prefix();
                    spans.push(Span::styled("  ", DIM));
                    spans.push(Span::styled(l.to_string(), Style::new().fg(CODE)));
                    self.lines.push(Line::from(spans));
                }
            }
            return;
        }
        if let Some((_, text)) = &mut self.link {
            text.push_str(t);
        }
        self.cur.push(Span::styled(t.to_string(), self.style()));
    }
}

pub fn markdown(md: &str) -> Vec<Line<'static>> {
    let mut r = Renderer {
        lines: Vec::new(),
        cur: Vec::new(),
        styles: Vec::new(),
        lists: Vec::new(),
        hang: 0,
        quote: 0,
        code: false,
        link: None,
        table: None,
        cell: String::new(),
    };
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => r.flush(),
                Tag::Heading { level, .. } => {
                    r.blank();
                    let color = if level == HeadingLevel::H1 { ACCENT } else { Color::Reset };
                    r.push_style(|s| s.fg(color).add_modifier(Modifier::BOLD));
                }
                Tag::BlockQuote(_) => {
                    r.flush();
                    r.quote += 1;
                    r.push_style(|s| s.add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    r.blank();
                    if let CodeBlockKind::Fenced(lang) = kind
                        && !lang.is_empty()
                    {
                        let mut spans = r.prefix();
                        spans.push(Span::styled(format!("  {lang}"), DIM));
                        r.lines.push(Line::from(spans));
                    }
                    r.code = true;
                }
                Tag::List(start) => {
                    if r.lists.is_empty() {
                        r.blank();
                    } else {
                        r.flush();
                    }
                    r.lists.push(start);
                }
                Tag::Item => {
                    r.flush();
                    let depth = r.lists.len().saturating_sub(1);
                    let marker = match r.lists.last_mut() {
                        Some(Some(n)) => {
                            *n += 1;
                            format!("{}. ", *n - 1)
                        }
                        _ => "• ".to_string(),
                    };
                    let indent = "  ".repeat(depth);
                    r.hang = indent.len() + marker.chars().count();
                    r.cur.push(Span::styled(format!("\u{0}{indent}{marker}"), Style::new().fg(ACCENT)));
                }
                Tag::Emphasis => r.push_style(|s| s.add_modifier(Modifier::ITALIC)),
                Tag::Strong => r.push_style(|s| s.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => r.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } => {
                    r.link = Some((dest_url.to_string(), String::new()));
                    r.push_style(|s| s.fg(CODE).add_modifier(Modifier::UNDERLINED));
                }
                Tag::Table(_) => {
                    r.blank();
                    r.table = Some(Vec::new());
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(t) = &mut r.table {
                        t.push(Vec::new());
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if r.lists.is_empty() {
                        r.blank();
                    } else {
                        r.flush();
                    }
                }
                TagEnd::Heading(_) => {
                    r.styles.pop();
                    r.blank();
                }
                TagEnd::BlockQuote(_) => {
                    r.styles.pop();
                    r.flush();
                    r.quote -= 1;
                    r.blank();
                }
                TagEnd::CodeBlock => {
                    r.code = false;
                    r.blank();
                }
                TagEnd::List(_) => {
                    r.flush();
                    r.lists.pop();
                    if r.lists.is_empty() {
                        r.hang = 0;
                        r.blank();
                    }
                }
                TagEnd::Item => r.flush(),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    r.styles.pop();
                }
                TagEnd::Link => {
                    r.styles.pop();
                    if let Some((url, text)) = r.link.take()
                        && url != text
                        && !url.starts_with('#')
                    {
                        r.cur.push(Span::styled(format!(" ({url})"), DIM));
                    }
                }
                TagEnd::TableCell => {
                    let cell = std::mem::take(&mut r.cell).trim().to_string();
                    if let Some(row) = r.table.as_mut().and_then(|t| t.last_mut()) {
                        row.push(cell);
                    }
                }
                TagEnd::Table => {
                    if let Some(rows) = r.table.take() {
                        r.table(rows);
                    }
                    r.blank();
                }
                _ => {}
            },
            Event::Text(t) => r.text(&t),
            Event::Code(t) if r.table.is_some() => r.cell.push_str(&t),
            Event::Code(t) => r.cur.push(Span::styled(t.to_string(), Style::new().fg(CODE))),
            Event::SoftBreak | Event::HardBreak => r.flush(),
            Event::Rule => {
                r.blank();
                r.lines.push(Line::styled("─".repeat(40), DIM));
                r.blank();
            }
            Event::TaskListMarker(done) => r.cur.push(Span::styled(if done { "☑ " } else { "☐ " }, Style::new().fg(ACCENT))),
            Event::Html(t) | Event::InlineHtml(t) => r.text(&t),
            _ => {}
        }
    }
    r.flush();
    while r.lines.last().is_some_and(|l| l.spans.is_empty()) {
        r.lines.pop();
    }
    r.lines
}

/// Word-wraps a line to `width` columns; continuation rows are indented like the line's
/// leading whitespace and list marker.
pub fn wrap(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(10);
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            let style = line.style.patch(s.style);
            s.content.chars().map(move |c| (c, style))
        })
        .collect();
    let total: usize = chars.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
    if total <= width {
        return vec![line.clone()];
    }
    let text: String = chars.iter().map(|(c, _)| *c).collect();
    let lead = text.chars().take_while(|c| *c == ' ' || *c == '│').count();
    let after = &text.chars().skip(lead).collect::<String>();
    let marker = if after.starts_with("• ") || after.starts_with("☐ ") || after.starts_with("☑ ") {
        2
    } else {
        after.find(". ").filter(|&i| i <= 3 && after[..i].chars().all(|c| c.is_ascii_digit())).map(|i| i + 2).unwrap_or(0)
    };
    let hang = (lead + marker).min(width / 2);

    let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut col = 0;
    let mut last_space: Option<usize> = None;
    for &(c, style) in &chars {
        let w = c.width().unwrap_or(0);
        if col + w > width {
            let row = rows.last_mut().unwrap();
            let carry: Vec<(char, Style)> = match last_space {
                Some(i) if i > hang => {
                    let rest = row.split_off(i + 1);
                    row.pop(); // the space itself
                    rest
                }
                _ => Vec::new(),
            };
            let mut next: Vec<(char, Style)> = (0..hang).map(|_| (' ', Style::default())).collect();
            next.extend(carry);
            col = next.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
            rows.push(next);
            last_space = None;
        }
        let row = rows.last_mut().unwrap();
        if c == ' ' {
            last_space = Some(row.len());
        }
        row.push((c, style));
        col += w;
    }
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, style) in row {
                match spans.last_mut() {
                    Some(s) if s.style == style => s.content.to_mut().push(c),
                    _ => spans.push(Span::styled(c.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn renders_blocks() {
        let md = "# Title\n\nSome **bold** and `code`.\nNext line.\n\n- one\n- two\n  1. nested\n\n```rust\nlet x = 1;\n```\n\n> quoted";
        let out = plain(&markdown(md));
        assert_eq!(
            out,
            vec![
                "Title", "", "Some bold and code.", "Next line.", "", "• one", "• two", "  1. nested", "", "  rust", "  let x = 1;", "",
                "│ quoted"
            ]
        );
        let table = plain(&markdown("| name | value |\n|---|---|\n| alpha | 1 |"));
        assert_eq!(table, vec!["name   value", "─────  ─────", "alpha  1"]);
    }

    #[test]
    fn wraps_with_hanging_indent() {
        let line = Line::from("• alpha beta gamma delta epsilon");
        let rows = plain(&wrap(&line, 14));
        assert_eq!(rows, vec!["• alpha beta", "  gamma delta", "  epsilon"]);
        assert!(rows.iter().all(|r| r.chars().count() <= 14));
    }
}
