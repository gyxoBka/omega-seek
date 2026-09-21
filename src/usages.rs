//! Where an identifier is declared and where it is used -- or where any text
//! is written: an error message, a route, a configuration key.
//!
//! What grep gives, minus what makes grep expensive to read: whole-word
//! matches only, comment lines dropped, the declaration first, every other
//! line labelled with the declaration it sits in, tests last, and a bound on
//! how much is printed. Matching is by name, not by type: two unrelated things
//! with one name are reported together.

use crate::index::{Index, Kind};
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::fmt::Write as _;

#[derive(Clone, Debug)]
pub struct Options {
    /// Lines printed at most; the rest are counted per file.
    pub limit: usize,
    /// Only paths containing this.
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
    /// The declaration this line sits in, if one precedes it in the file.
    inside: Option<String>,
    declares: bool,
}

struct FileUsages {
    path: String,
    kind: Kind,
    lines: Vec<Line>,
}

/// What is being looked for.
#[derive(Clone, Copy)]
enum Needle<'a> {
    /// An identifier: whole-word matches in code, comment lines dropped.
    Word(&'a str),
    /// Anything else -- an error message, a route, a config key: the text as
    /// written, wherever it is written.
    Text(&'a str),
}

impl Needle<'_> {
    fn text(&self) -> &str {
        match self {
            Self::Word(text) | Self::Text(text) => text,
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
    // `Session::refresh`, `$this->check`, `pkg.Validate`: a path of identifiers asks
    // for the last of them. Anything with a space, a slash or a quote in it is
    // text to be found as written.
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

    let mut found = collect(index, needle, options.path.as_deref());
    if found.is_empty() {
        return match needle {
            Needle::Word(name) => format!(
                "No whole-word occurrences of `{name}` in code. Matching is case-sensitive; use search for a fuzzy lookup."
            ),
            Needle::Text(text) => format!(
                "No occurrences of `{text}` as written. Matching is exact and case-sensitive; try a shorter fragment, or search."
            ),
        };
    }
    render(&index.label, needle.text(), &mut found, options)
}

/// How many lines other than its declarations name `symbol`, and in how many files.
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

/// Every line that holds the needle, by file.
fn collect(index: &Index, needle: Needle, path: Option<&str>) -> Vec<FileUsages> {
    let symbol = needle.text();
    // A file that holds the needle holds every term of it, so the postings say
    // which files are worth reading: those of the rarest term, narrowed by the
    // others. Text with no terms at all -- punctuation -- is looked for everywhere.
    //
    // Text may begin and end mid-word (`fig/ap`), and half a word is no term,
    // so only its inner words narrow the candidates.
    let mut terms = Vec::new();
    match needle {
        Needle::Word(name) => index.tokenizer.terms(name, &mut terms),
        Needle::Text(text) => {
            let words: Vec<&str> = text.split_whitespace().collect();
            if words.len() > 2 {
                index.tokenizer.terms(&words[1..words.len() - 1].join(" "), &mut terms);
            }
        }
    }
    let files_of = |term: &String| -> Option<BTreeSet<u32>> {
        let postings = index.postings.get(term)?;
        Some(postings.iter().map(|&(chunk, _)| index.chunks[chunk as usize].file).collect())
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
                Needle::Word(_) => matches!(entry.kind, Kind::Code | Kind::Test),
                // A route or a key is as likely to sit in configuration.
                Needle::Text(_) => entry.kind != Kind::Docs,
            };
            searched && path.is_none_or(|wanted| entry.path.contains(wanted))
        })
        .collect();

    // Every declaration of each candidate file, by line, to label lines with.
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
                .filter(|(_, line)| match needle {
                    Needle::Word(name) => !is_comment(line) && has_word(line, name),
                    Needle::Text(text) => line.contains(text),
                })
                .map(|(offset, line)| {
                    let number = offset as u32 + 1;
                    let declares = declared.iter().any(|&(at, name)| at == number && name == symbol);
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

fn render(label: &str, symbol: &str, found: &mut [FileUsages], options: &Options) -> String {
    // Code before tests, and within each the files that use it most.
    found.sort_by(|a, b| {
        (a.kind == Kind::Test, std::cmp::Reverse(a.lines.len()), &a.path)
            .cmp(&(b.kind == Kind::Test, std::cmp::Reverse(b.lines.len()), &b.path))
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
    out.push_str("\nUsed:\n");
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

/// Lines of one file printed in full; the rest are named by number.
const PER_FILE: usize = 8;
/// Files named, with their counts, once the line budget is spent.
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

/// Whether `word` occurs in `line` with no identifier character on either side.
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
}
