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
    /// A regular expression, tried on every line of every indexed file: what
    /// grep is reached for, without the dependencies, build output and
    /// generated files that grep also walks into.
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

    // `cart.clear` has the shape of `pkg.Validate` and may be neither: the name
    // of an operation, an event, a config key. Code that writes it in quotes
    // settles it, and then its last word alone would answer about every `clear`.
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
        // Outside `path` the dotted name may be written in quotes after all.
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

/// Every line a regular expression matches, in the indexed files only: grep
/// without the dependencies, build output and generated files grep walks into,
/// and with what `usages` adds -- the declaration each line sits in, tests
/// last, a bound on what is printed.
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

/// What to say when nothing was found under `path`: "none here" must not read
/// as "none anywhere", or the agent concludes the thing does not exist.
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

/// Whether `line` writes `text` as a string, or as the head or tail of one.
fn is_quoted(line: &str, text: &str) -> bool {
    let quote = |c: char| matches!(c, '"' | '\'' | '`');
    line.match_indices(text).any(|(at, _)| {
        line[..at].chars().next_back().is_some_and(quote) || line[at + text.len()..].chars().next().is_some_and(quote)
    })
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
        // Which words an expression requires is not worth working out for a
        // scan that takes tens of milliseconds.
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
                // An identifier is also written in the documentation -- a
                // task id in a registry, a name in a design note -- and those
                // lines come after the code's.
                Needle::Word(_) => entry.kind != Kind::Config,
                // A route or a key is as likely to sit in configuration, and
                // a phrase in the documentation.
                Needle::Text(_) => true,
                Needle::Pattern(_) => true,
            };
            searched && path.is_none_or(|wanted| entry.path.contains(wanted))
        })
        .collect();

    // `.btn-primary` and `#site-header` are declared as the name without its mark.
    let bare = symbol.trim_start_matches(['.', '#', '%']);
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
                .filter(|(offset, line)| match needle {
                    // A `#` opens a comment in code and a heading in a document.
                    Needle::Word(name) => (entry.kind == Kind::Docs || !is_comment(line)) && has_word(line, name),
                    // A nested rule declares a name its line does not spell:
                    // `&__title` under `.card` is where `card__title` is.
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
    // Code before tests, and within each the files that use it most.
    // Code before tests before documents, and within each the files that use it most.
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

    #[test]
    fn a_dotted_name_in_quotes_is_text() {
        assert!(is_quoted(r#"call("cart.clear", input)"#, "cart.clear"));
        assert!(is_quoted("| 'cart.clear'", "cart.clear"));
        assert!(is_quoted(r#"on("cart.clear.done")"#, "cart.clear"));
        assert!(!is_quoted("cart.clear()", "cart.clear"));
    }
}
