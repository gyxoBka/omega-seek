//! Cutting a file into chunks along the boundaries its own layout shows.
//!
//! No grammar is involved. In every language worth indexing a declaration
//! starts at the shallowest indentation of its surroundings, after a blank line
//! or after the line that closed the previous one; that is enough to keep a
//! function, its doc comment and its attributes in one piece.

/// A run of lines, zero-based, end exclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// Units shorter than this are joined with their neighbours.
const MIN_LINES: usize = 12;
/// Units longer than this are cut again one level deeper.
const MAX_LINES: usize = 80;
const WINDOW: usize = 60;
const WINDOW_STRIDE: usize = 50;
const MAX_DEPTH: usize = 4;

#[derive(Clone, Copy)]
struct Line {
    indent: usize,
    blank: bool,
    closer: bool,
}

#[must_use]
pub fn chunk(lines: &[&str]) -> Vec<Span> {
    let info: Vec<Line> = lines.iter().map(|line| describe(line)).collect();
    let mut out = Vec::new();
    split(&info, 0, lines.len(), 0, &mut out);
    out
}

fn describe(line: &str) -> Line {
    let trimmed = line.trim_start();
    let indent = line[..line.len() - trimmed.len()]
        .chars()
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    let body = trimmed.trim_end();
    Line {
        indent,
        blank: body.is_empty(),
        closer: body.starts_with(['}', ')', ']']) || body == "end",
    }
}

fn split(info: &[Line], lo: usize, hi: usize, depth: usize, out: &mut Vec<Span>) {
    let (lo, hi) = trim_blank(info, lo, hi);
    if lo >= hi {
        return;
    }
    let units = units(info, lo, hi);
    if units.len() <= 1 {
        windows(lo, hi, out);
        return;
    }
    let mut pending: Option<Span> = None;
    for unit in units {
        if unit.end - unit.start > MAX_LINES && depth < MAX_DEPTH {
            if let Some(span) = pending.take() {
                out.push(span);
            }
            // The opening line stays with the first piece of what it opens.
            let before = out.len();
            split(info, unit.start + 1, unit.end, depth + 1, out);
            match out.get_mut(before) {
                Some(first) => first.start = unit.start,
                None => out.push(unit),
            }
            continue;
        }
        let joined = match pending {
            Some(span) => Span {
                start: span.start,
                end: unit.end,
            },
            None => unit,
        };
        if joined.end - joined.start >= MIN_LINES {
            out.push(joined);
            pending = None;
        } else {
            pending = Some(joined);
        }
    }
    // A few trailing lines are not worth a chunk of their own: alone they are
    // short enough for length normalisation to rank them above real answers.
    if let Some(span) = pending {
        match out.last_mut() {
            Some(last)
                if span.end - span.start < MIN_LINES / 2
                    && last.end <= span.start
                    && last.end - last.start < MAX_LINES =>
            {
                last.end = span.end;
            }
            _ => out.push(span),
        }
    }
}

/// The range cut at every line that starts something at its shallowest level.
fn units(info: &[Line], lo: usize, hi: usize) -> Vec<Span> {
    let base = info[lo..hi]
        .iter()
        .filter(|line| !line.blank && !line.closer)
        .map(|line| line.indent)
        .min()
        .unwrap_or(0);
    let mut out = Vec::new();
    let mut start = lo;
    for i in lo + 1..hi {
        let line = info[i];
        let prev = info[i - 1];
        let opens = !line.blank
            && !line.closer
            && line.indent == base
            && (prev.blank || (prev.closer && prev.indent <= base));
        if opens {
            let (s, e) = trim_blank(info, start, i);
            if s < e {
                out.push(Span { start: s, end: e });
            }
            start = i;
        }
    }
    let (s, e) = trim_blank(info, start, hi);
    if s < e {
        out.push(Span { start: s, end: e });
    }
    out
}

fn windows(lo: usize, hi: usize, out: &mut Vec<Span>) {
    if hi - lo <= MAX_LINES {
        out.push(Span { start: lo, end: hi });
        return;
    }
    let mut start = lo;
    loop {
        let end = (start + WINDOW).min(hi);
        out.push(Span { start, end });
        if end == hi {
            break;
        }
        start += WINDOW_STRIDE;
    }
}

fn trim_blank(info: &[Line], mut lo: usize, mut hi: usize) -> (usize, usize) {
    while lo < hi && info[lo].blank {
        lo += 1;
    }
    while hi > lo && info[hi - 1].blank {
        hi -= 1;
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_function_keeps_its_comment_and_small_items_join() {
        let mut source = String::from("use a;\nuse b;\n\n");
        for name in ["one", "two"] {
            source.push_str(&format!("/// About {name}.\nfn {name}() {{\n"));
            source.push_str(&"    body();\n".repeat(14));
            source.push_str("}\n\n");
        }
        let lines: Vec<&str> = source.lines().collect();
        let spans = chunk(&lines);
        assert_eq!(spans.len(), 2);
        assert_eq!(lines[spans[0].start], "use a;");
        assert_eq!(lines[spans[1].start], "/// About two.");
        assert_eq!(lines[spans[1].end - 1], "}");
    }

    #[test]
    fn a_long_block_is_cut_by_its_members() {
        let mut source = String::from("impl Thing {\n");
        for i in 0..6 {
            source.push_str(&format!("    fn member_{i}() {{\n"));
            source.push_str(&"        body();\n".repeat(18));
            source.push_str("    }\n\n");
        }
        source.push_str("}\n\nfn after() {}\n");
        let lines: Vec<&str> = source.lines().collect();
        let spans = chunk(&lines);
        assert_eq!(spans[0].start, 0);
        assert!(lines[spans[1].start].contains("member_1"));
        assert!(spans.len() >= 6);
    }
}
