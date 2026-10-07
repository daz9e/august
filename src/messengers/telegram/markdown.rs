//! Markdown -> Telegram HTML (the `parse_mode: HTML` subset), plus a splitter that
//! cuts long Markdown into message-sized pieces without breaking code fences.

use crate::messengers::chunk::{fence_open, is_fence_close};

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}


fn is_table_line(l: &str) -> bool {
    let t = l.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() > 1
}

fn is_hr(l: &str) -> bool {
    let t: String = l.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

pub fn to_html(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        if let Some((ticks, lang)) = fence_open(line) {
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !is_fence_close(lines[i], ticks) {
                code.push(lines[i]);
                i += 1;
            }
            i += 1; // closing fence (or end of input while streaming)
            let body = escape(&code.join("\n"));
            let lang = lang.split_whitespace().next().unwrap_or("");
            out.push(if lang.is_empty() || !lang.chars().all(|c| c.is_ascii_alphanumeric() || "+-_#.".contains(c)) {
                format!("<pre>{body}</pre>")
            } else {
                format!("<pre><code class=\"language-{lang}\">{body}</code></pre>")
            });
            continue;
        }

        if is_table_line(line) {
            let mut rows = Vec::new();
            while i < lines.len() && is_table_line(lines[i]) {
                let cells: Vec<&str> = lines[i].trim().trim_matches('|').split('|').map(str::trim).collect();
                let separator = cells.iter().all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'));
                if !separator {
                    rows.push(cells.join(" | "));
                }
                i += 1;
            }
            out.push(format!("<pre>{}</pre>", escape(&rows.join("\n"))));
            continue;
        }

        if line.trim_start().starts_with('>') {
            let mut quote = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                let t = lines[i].trim_start().trim_start_matches('>');
                quote.push(inline(t.strip_prefix(' ').unwrap_or(t)));
                i += 1;
            }
            out.push(format!("<blockquote>{}</blockquote>", quote.join("\n")));
            continue;
        }

        out.push(block_line(line));
        i += 1;
    }
    out.join("\n")
}

fn block_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        return format!("<b>{}</b>", inline(trimmed[hashes..].trim()));
    }
    if is_hr(trimmed) {
        return "──────────".into();
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            let (mark, rest) = match rest.strip_prefix("[ ] ") {
                Some(r) => ("☐ ", r),
                None => match rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
                    Some(r) => ("☑ ", r),
                    None => ("• ", rest),
                },
            };
            return format!("{indent}{mark}{}", inline(rest));
        }
    }
    format!("{indent}{}", inline(trimmed))
}

fn find_double(c: &[char], from: usize, d: char) -> Option<usize> {
    (from..c.len().saturating_sub(1)).find(|&j| c[j] == d && c[j + 1] == d)
}

fn find_single(c: &[char], from: usize, d: char) -> Option<usize> {
    let mut j = from;
    while j < c.len() {
        if c[j] == '\\' {
            j += 2;
            continue;
        }
        if c[j] == d {
            if c.get(j + 1) == Some(&d) {
                j += 2; // skip a nested double marker
                continue;
            }
            return Some(j);
        }
        j += 1;
    }
    None
}

fn safe_url(u: &str) -> bool {
    ["http://", "https://", "tg://", "mailto:"].iter().any(|p| u.starts_with(p))
}

pub fn inline(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut o = String::new();
    let mut i = 0;
    let span = |a: usize, b: usize| c[a..b].iter().collect::<String>();
    while i < n {
        let ch = c[i];
        match ch {
            '\\' if i + 1 < n && c[i + 1].is_ascii_punctuation() => {
                o.push_str(&escape(&c[i + 1].to_string()));
                i += 2;
                continue;
            }
            '`' => {
                let ticks = c[i..].iter().take_while(|&&x| x == '`').count();
                let mut j = i + ticks;
                let mut end = None;
                while j < n {
                    let run = c[j..].iter().take_while(|&&x| x == '`').count();
                    if run == ticks {
                        end = Some(j);
                        break;
                    }
                    j += run.max(1);
                }
                if let Some(e) = end {
                    o.push_str(&format!("<code>{}</code>", escape(span(i + ticks, e).trim())));
                    i = e + ticks;
                    continue;
                }
            }
            '*' | '_' | '~' if c.get(i + 1) == Some(&ch) => {
                if let Some(e) = find_double(&c, i + 2, ch) {
                    let inner = span(i + 2, e);
                    if !inner.trim().is_empty() && !inner.starts_with(char::is_whitespace) {
                        let tag = if ch == '~' { "s" } else { "b" };
                        o.push_str(&format!("<{tag}>{}</{tag}>", inline(&inner)));
                        i = e + 2;
                        continue;
                    }
                }
            }
            '*' | '_' => {
                let word_before = i > 0 && c[i - 1].is_alphanumeric();
                let opens = c.get(i + 1).is_some_and(|x| !x.is_whitespace());
                if opens && !(ch == '_' && word_before) {
                    if let Some(e) = find_single(&c, i + 1, ch) {
                        let word_after = c.get(e + 1).is_some_and(|x| x.is_alphanumeric());
                        if e > i + 1 && !c[e - 1].is_whitespace() && !(ch == '_' && word_after) {
                            o.push_str(&format!("<i>{}</i>", inline(&span(i + 1, e))));
                            i = e + 1;
                            continue;
                        }
                    }
                }
            }
            '[' => {
                if let Some(mid) = (i + 1..n.saturating_sub(1)).find(|&j| c[j] == ']' && c[j + 1] == '(') {
                    if let Some(close) = (mid + 2..n).find(|&j| c[j] == ')') {
                        let url = span(mid + 2, close);
                        let url = url.trim();
                        if safe_url(url) {
                            o.push_str(&format!(
                                "<a href=\"{}\">{}</a>",
                                escape(url),
                                inline(&span(i + 1, mid))
                            ));
                            i = close + 1;
                            continue;
                        }
                    }
                }
            }
            _ => {}
        }
        o.push_str(&escape(&ch.to_string()));
        i += 1;
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_styles() {
        assert_eq!(inline("**bold** and *it* and ~~del~~"), "<b>bold</b> and <i>it</i> and <s>del</s>");
        assert_eq!(inline("a `x<y` b"), "a <code>x&lt;y</code> b");
        assert_eq!(inline("snake_case_name stays"), "snake_case_name stays");
        assert_eq!(inline("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(inline("[ok](https://a.b/c?x=1&y=2)"), "<a href=\"https://a.b/c?x=1&amp;y=2\">ok</a>");
        assert_eq!(inline("[bad](javascript:x)"), "[bad](javascript:x)");
        assert_eq!(inline("1 < 2 & 3 > 2"), "1 &lt; 2 &amp; 3 &gt; 2");
        assert_eq!(inline("**unclosed"), "**unclosed");
        assert_eq!(inline("\\*lit\\*"), "*lit*");
        assert_eq!(inline("**bold _and it_**"), "<b>bold <i>and it</i></b>");
    }

    #[test]
    fn blocks() {
        let md = "# Title\n- one\n- [x] done\n1. first\n> quote\n\n```rust\nlet a = 1 < 2;\n```\n| a | b |\n|---|---|\n| 1 | 2 |";
        let h = to_html(md);
        assert!(h.starts_with("<b>Title</b>\n• one\n☑ done\n1. first\n<blockquote>quote</blockquote>"));
        assert!(h.contains("<pre><code class=\"language-rust\">let a = 1 &lt; 2;</code></pre>"));
        assert!(h.contains("<pre>a | b\n1 | 2</pre>"));
    }

    #[test]
    fn unterminated_fence_while_streaming() {
        assert_eq!(to_html("```\nfoo"), "<pre>foo</pre>");
    }
}
