use crate::index::{Index, Kind};
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::fmt::Write as _;

#[derive(Clone, Debug)]
pub struct Options {
    pub limit: usize,
    pub path: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limit: 40,
            path: None,
        }
    }
}

struct Line {
    number: u32,
    text: String,
    inside: Option<String>,
    declares: bool,
}

struct FileUsages {
    path: String,
    kind: Kind,
    lines: Vec<Line>,
}

#[derive(Clone, Copy)]
enum Needle<'a> {
    Word(&'a str),
    Text(&'a str),
    Pattern(&'a regex::Regex),
}

impl Needle<'_> {
    fn text(&self) -> &str {
        match self {
            Self::Word(text) | Self::Text(text) => text,
            Self::Pattern(pattern) => pattern.as_str(),
        }
    }
}

#[must_use]
pub fn usages(index: &Index, asked: &str, options: &Options) -> String {
    if index.files.is_empty() {
        return crate::search::nothing_indexed(index);
    }
    let asked = asked.trim();
    if asked.is_empty() {
        return "`symbol` is empty.".to_owned();
    }
    let is_part = |c: char| c.is_alphanumeric() || c == '_';
    let path_of_names = asked
        .split("::")
        .flat_map(|part| part.split("->"))
        .flat_map(|part| part.split(['.', '\\', '$']))
        .all(|part| part.chars().all(is_part));
    let needle = match asked.split(|c: char| !is_part(c)).filter(|part| !part.is_empty()).next_back() {
        Some(name) if path_of_names => Needle::Word(name),
        _ => Needle::Text(asked),
    };

    let qualified = matches!(needle, Needle::Word(name) if name != asked);
    if qualified {
        let mut written = collect(index, Needle::Text(asked), options.path.as_deref());
        if written.iter().flat_map(|file| &file.lines).any(|line| is_quoted(&line.text, asked)) {
            let mut out = render(&index.label, asked, "Used", &mut written, options);
            let _ = write!(
                out,
                "\nFound as text, since the code writes `{asked}` in quotes; ask for `{}` alone for the identifier.\n",
                needle.text()
            );
            return out;
        }
    }

    let mut found = collect(index, needle, options.path.as_deref());
    if found.is_empty() {
        if qualified && options.path.is_some() {
            let written = collect(index, Needle::Text(asked), None);
            if written.iter().flat_map(|file| &file.lines).any(|line| is_quoted(&line.text, asked)) {
                return elsewhere(index, Needle::Text(asked), options.path.as_deref()).unwrap_or_default();
            }
        }
        if let Some(outside) = elsewhere(index, needle, options.path.as_deref()) {
            return outside;
        }
        return match needle {
            Needle::Word(name) if qualified => format!(
                "`{asked}` is not written in quotes anywhere, and there are no whole-word occurrences of `{name}` in code or documents. \
                 Matching is case-sensitive; use search for a fuzzy lookup."
            ),
            Needle::Word(name) => format!(
                "No whole-word occurrences of `{name}` in code or documents. Matching is case-sensitive; use search for a fuzzy lookup."
            ),
            Needle::Text(text) => format!(
                "No occurrences of `{text}` as written. Matching is exact and case-sensitive; try a shorter fragment, or search."
            ),
            Needle::Pattern(_) => String::new(),
        };
    }
    let mut out = String::new();
    if qualified {
        let _ = writeln!(out, "`{asked}` read as the identifier `{}`.", needle.text());
    }
    out.push_str(&render(&index.label, needle.text(), "Used", &mut found, options));
    out
}

#[must_use]
pub fn grep(index: &Index, asked: &str, options: &Options) -> String {
    if index.files.is_empty() {
        return crate::search::nothing_indexed(index);
    }
    if asked.is_empty() {
        return "`pattern` is empty.".to_owned();
    }
    let pattern = match regex::Regex::new(asked) {
        Ok(pattern) => pattern,
        Err(reason) => return format!("`{asked}` is not a regular expression: {reason}"),
    };
    let mut found = collect(index, Needle::Pattern(&pattern), options.path.as_deref());
    if found.is_empty() {
        if let Some(outside) = elsewhere(index, Needle::Pattern(&pattern), options.path.as_deref()) {
            return outside;
        }
        return format!(
            "No line matches /{asked}/ in the indexed files. Lines are matched one at a time, \
             case-sensitively unless the expression opens with (?i)."
        );
    }
    render(&index.label, &format!("/{asked}/"), "Matched", &mut found, options)
}

fn elsewhere(index: &Index, needle: Needle, path: Option<&str>) -> Option<String> {
    let path = path?;
    let shown = match needle {
        Needle::Pattern(pattern) => format!("/{}/", pattern.as_str()),
        _ => format!("`{}`", needle.text()),
    };
    if !index.files.iter().any(|file| file.path.contains(path)) {
        return Some(format!("No indexed file has `{path}` in its path, so {shown} was not looked for."));
    }
    let found = collect(index, needle, None);
    if found.is_empty() {
        return None;
    }
    let lines: usize = found.iter().map(|file| file.lines.len()).sum();
    let mut files: Vec<&FileUsages> = found.iter().collect();
    files.sort_by_key(|file| (file.kind == Kind::Test, std::cmp::Reverse(file.lines.len())));
    let named: Vec<&str> = files.iter().take(3).map(|file| file.path.as_str()).collect();
    Some(format!(
        "Nothing for {shown} under `{path}`, but {lines} line{} in {} file{} elsewhere: {}{}. Drop or change `path`.",
        plural(lines),
        found.len(),
        plural(found.len()),
        named.join(", "),
        if found.len() > named.len() { ", ..." } else { "" },
    ))
}

fn is_quoted(line: &str, text: &str) -> bool {
    let quote = |c: char| matches!(c, '"' | '\'' | '`');
    line.match_indices(text).any(|(at, _)| {
        line[..at].chars().next_back().is_some_and(quote) || line[at + text.len()..].chars().next().is_some_and(quote)
    })
}

#[must_use]
pub fn count(index: &Index, symbol: &str) -> (usize, usize) {
    let found = collect(index, Needle::Word(symbol), None);
    let others = found
        .iter()
        .flat_map(|file| &file.lines)
        .filter(|line| !line.declares)
        .count();
    (others, found.len())
}

fn collect(index: &Index, needle: Needle, path: Option<&str>) -> Vec<FileUsages> {
    let symbol = needle.text();
    let mut terms = Vec::new();
    match needle {
        Needle::Word(name) => index.tokenizer.terms(name, &mut terms),
        Needle::Text(text) => {
            let words: Vec<&str> = text.split_whitespace().collect();
            if words.len() > 2 {
                index.tokenizer.terms(&words[1..words.len() - 1].join(" "), &mut terms);
            }
        }
        Needle::Pattern(_) => {}
    }
    let files_of = |term: &String| -> Option<BTreeSet<u32>> {
        let postings = index.postings(term);
        (!postings.is_empty()).then(|| postings.iter().map(|&(chunk, _)| index.chunks[chunk as usize].file).collect())
    };
    let mut candidates: Option<BTreeSet<u32>> = None;
    for term in &terms {
        let Some(files) = files_of(term) else {
            return Vec::new();
        };
        candidates = Some(match candidates {
            Some(known) => known.intersection(&files).copied().collect(),
            None => files,
        });
    }
    let candidates: BTreeSet<u32> = candidates
        .unwrap_or_else(|| (0..index.files.len() as u32).collect())
        .into_iter()
        .filter(|&file| {
            let entry = &index.files[file as usize];
            let searched = match needle {
                Needle::Word(_) => entry.kind != Kind::Config,
                Needle::Text(_) => true,
                Needle::Pattern(_) => true,
            };
            searched && path.is_none_or(|wanted| entry.path.contains(wanted))
        })
        .collect();

    let bare = symbol.trim_start_matches(['.', '#', '%']);
    let mut declared: Vec<Vec<(u32, &str)>> = vec![Vec::new(); index.files.len()];
    for chunk in &index.chunks {
        if candidates.contains(&chunk.file) {
            declared[chunk.file as usize]
                .extend(chunk.name_lines.iter().copied().zip(chunk.names.iter().map(String::as_str)));
        }
    }

    candidates
        .par_iter()
        .filter_map(|&file| {
            let entry = &index.files[file as usize];
            let text = std::fs::read_to_string(index.root.join(&entry.path)).ok()?;
            let declared = &declared[file as usize];
            let lines: Vec<Line> = text
                .lines()
                .enumerate()
                .filter(|(offset, line)| match needle {
                    Needle::Word(name) => (entry.kind == Kind::Docs || !is_comment(line)) && has_word(line, name),
                    Needle::Text(text) => {
                        line.contains(text)
                            || declared.iter().any(|&(at, name)| at == *offset as u32 + 1 && name == bare)
                    }
                    Needle::Pattern(pattern) => pattern.is_match(line),
                })
                .map(|(offset, line)| {
                    let number = offset as u32 + 1;
                    let declares = declared.iter().any(|&(at, name)| at == number && (name == symbol || name == bare));
                    let inside = declared
                        .iter()
                        .filter(|&&(at, name)| at <= number && name != symbol)
                        .next_back()
                        .map(|&(_, name)| name.to_owned());
                    Line {
                        number,
                        text: line.trim().to_owned(),
                        inside,
                        declares,
                    }
                })
                .collect();
            (!lines.is_empty()).then(|| FileUsages {
                path: entry.path.clone(),
                kind: entry.kind,
                lines,
            })
        })
        .collect()
}

fn render(label: &str, symbol: &str, listed: &str, found: &mut [FileUsages], options: &Options) -> String {
    let order = |kind: Kind| match kind {
        Kind::Code | Kind::Config => 0,
        Kind::Test => 1,
        Kind::Docs => 2,
    };
    found.sort_by(|a, b| {
        (order(a.kind), std::cmp::Reverse(a.lines.len()), &a.path)
            .cmp(&(order(b.kind), std::cmp::Reverse(b.lines.len()), &b.path))
    });

    let declarations: usize = found.iter().flat_map(|file| &file.lines).filter(|line| line.declares).count();
    let total: usize = found.iter().map(|file| file.lines.len()).sum();
    let mut out = if declarations == 0 {
        format!("`{symbol}`: {total} line{} in {} file{}\n", plural(total), found.len(), plural(found.len()))
    } else {
        format!(
            "`{symbol}`: {declarations} declaration{}, {} other line{} in {} file{}\n",
            plural(declarations),
            total - declarations,
            plural(total - declarations),
            found.len(),
            plural(found.len()),
        )
    };

    let mut budget = options.limit;
    if declarations > 0 {
        out.push_str("\nDeclared:\n");
        for file in found.iter() {
            for line in file.lines.iter().filter(|line| line.declares) {
                let _ = writeln!(out, "{label}{}:{}  {}", file.path, line.number, clip(&line.text));
                budget = budget.saturating_sub(1);
            }
        }
    }
    let _ = write!(out, "\n{listed}:\n");
    let mut unshown: Vec<String> = Vec::new();
    for file in found.iter() {
        let uses: Vec<&Line> = file.lines.iter().filter(|line| !line.declares).collect();
        if uses.is_empty() {
            continue;
        }
        if budget == 0 {
            unshown.push(format!("{} ({})", file.path, uses.len()));
            continue;
        }
        let _ = writeln!(out, "{label}{}", file.path);
        let shown = uses.len().min(budget).min(PER_FILE);
        for line in &uses[..shown] {
            let inside = line.inside.as_deref().map(|name| format!("[{name}] ")).unwrap_or_default();
            let _ = writeln!(out, "{:>6}  {inside}{}", line.number, clip(&line.text));
        }
        if shown < uses.len() {
            let rest: Vec<String> = uses[shown..].iter().map(|line| line.number.to_string()).collect();
            let _ = writeln!(out, "        also lines {}", rest.join(", "));
        }
        budget -= shown;
    }
    if !unshown.is_empty() {
        let more = unshown.len().saturating_sub(UNSHOWN_FILES);
        unshown.truncate(UNSHOWN_FILES);
        let _ = write!(out, "\nNot shown (pass `path` to see): {}", unshown.join(", "));
        if more > 0 {
            let _ = write!(out, ", and {more} more file{}", plural(more));
        }
        out.push('\n');
    }
    out
}

const PER_FILE: usize = 8;
const UNSHOWN_FILES: usize = 10;
const MAX_LINE_CHARS: usize = 160;

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let mut clipped: String = line.chars().take(MAX_LINE_CHARS).collect();
    clipped.push('…');
    clipped
}

fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with("* ")
        || line == "*"
        || line.starts_with("--")
        || (line.starts_with('#') && !line.starts_with("#[") && !line.starts_with("#!") && !line.starts_with("#include") && !line.starts_with("#define"))
}

fn has_word(line: &str, word: &str) -> bool {
    let is_part = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(word).any(|(at, _)| {
        !line[..at].chars().next_back().is_some_and(is_part)
            && !line[at + word.len()..].chars().next().is_some_and(is_part)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_not_a_part_of_a_longer_one() {
        assert!(has_word("session.ValidateToken(token)", "ValidateToken"));
        assert!(has_word("$this->check();", "check"));
        assert!(!has_word("recheck_all()", "check"));
        assert!(!has_word("ValidateTokenStrict()", "ValidateToken"));
    }

    #[test]
    fn a_dotted_name_in_quotes_is_text() {
        assert!(is_quoted(r#"call("cart.clear", input)"#, "cart.clear"));
        assert!(is_quoted("| 'cart.clear'", "cart.clear"));
        assert!(is_quoted(r#"on("cart.clear.done")"#, "cart.clear"));
        assert!(!is_quoted("cart.clear()", "cart.clear"));
    }
}
