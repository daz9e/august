//! Messenger-independent splitting of long Markdown into message-sized pieces.

pub(crate) fn fence_open(line: &str) -> Option<(&str, &str)> {
    let t = line.trim_start();
    let ticks = t.len() - t.trim_start_matches('`').len();
    (ticks >= 3).then(|| (&t[..ticks], t[ticks..].trim()))
}

pub(crate) fn is_fence_close(line: &str, open: &str) -> bool {
    let t = line.trim();
    t.len() >= open.len() && t.chars().all(|c| c == '`')
}

/// Cuts Markdown into pieces of at most `max` chars, preferring paragraph, then
/// line boundaries. A code fence cut in half is closed and reopened.
pub fn split_markdown(md: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let mut open_fence: Option<String> = None; // full opening line while inside a fence

    let push_line = |chunks: &mut Vec<String>, cur: &mut String, cur_len: &mut usize, open: &Option<String>, line: &str| {
        let len = line.chars().count() + 1;
        // Reserve room for a closing fence.
        let reserve = if open.is_some() { 4 } else { 0 };
        if *cur_len > 0 && *cur_len + len + reserve > max {
            if open.is_some() {
                cur.push_str("```");
            } else {
                while cur.ends_with('\n') {
                    cur.pop();
                }
            }
            chunks.push(std::mem::take(cur));
            *cur_len = 0;
            if let Some(f) = open {
                cur.push_str(f);
                cur.push('\n');
                *cur_len = f.chars().count() + 1;
            }
        }
        cur.push_str(line);
        cur.push('\n');
        *cur_len += len;
    };

    for line in md.lines() {
        // Hard-wrap lines longer than a whole message.
        let pieces: Vec<String> = if line.chars().count() >= max {
            let cs: Vec<char> = line.chars().collect();
            cs.chunks(max.saturating_sub(8).max(1)).map(|p| p.iter().collect()).collect()
        } else {
            vec![line.to_string()]
        };
        for p in &pieces {
            push_line(&mut chunks, &mut cur, &mut cur_len, &open_fence, p);
        }
        if let Some(open) = &open_fence {
            if is_fence_close(line, open.trim_start().trim_end_matches(|c: char| c != '`')) {
                open_fence = None;
            }
        } else if fence_open(line).is_some() {
            open_fence = Some(line.trim().to_string());
        }
    }
    while cur.ends_with('\n') {
        cur.pop();
    }
    if !cur.trim().is_empty() || chunks.is_empty() {
        chunks.push(cur);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_fences_balanced() {
        let md = format!("intro\n```py\n{}\n```\nouter", "x = 1\n".repeat(30));
        let parts = split_markdown(&md, 80);
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(p.chars().count() <= 80, "{}", p.chars().count());
            assert_eq!(p.matches("```").count() % 2, 0, "unbalanced: {p}");
        }
        assert!(parts.last().unwrap().ends_with("outer"));
    }

    #[test]
    fn split_short_and_long_lines() {
        assert_eq!(split_markdown("hi", 100), vec!["hi"]);
        let parts = split_markdown(&"a".repeat(250), 100);
        assert!(parts.iter().all(|p| p.chars().count() <= 100));
        assert_eq!(parts.concat().len(), 250);
    }
}
