//! The table of contents of a file or a directory.
//!
//! What a file declares, with line numbers, costs a few dozen lines to read;
//! the file itself costs thousands. An agent that sees the contents first can
//! decide whether the file matters and read only the part that does.

use crate::index::{Index, Kind};
use std::collections::BTreeMap;
use std::fmt::Write as _;

const MAX_SIGNATURE_CHARS: usize = 120;
const MAX_CANDIDATES: usize = 15;
const MAX_ENTRIES: usize = 80;
/// Names shown beside a file in a directory listing.
const NAMES_PER_FILE: usize = 6;

#[must_use]
pub fn outline(index: &Index, asked: &str) -> String {
    if index.files.is_empty() {
        return crate::search::nothing_indexed(index);
    }
    let asked = asked.trim().replace('\\', "/");
    let asked = asked.trim_start_matches("./").trim_matches('/');

    if asked.is_empty() || asked == "." {
        return directory(index, "");
    }
    if let Some(file) = index.files.iter().position(|file| file.path == asked) {
        return file_outline(index, file);
    }
    let prefix = format!("{asked}/");
    if index.files.iter().any(|file| file.path.starts_with(&prefix)) {
        return directory(index, &prefix);
    }
    // A bare file name, or the tail of a path.
    let mut matches: Vec<usize> = (0..index.files.len())
        .filter(|&file| {
            let path = &index.files[file].path;
            path.ends_with(&format!("/{asked}")) || path.ends_with(asked)
        })
        .collect();
    if matches.is_empty() {
        matches = (0..index.files.len())
            .filter(|&file| index.files[file].path.contains(asked))
            .collect();
    }
    match matches.as_slice() {
        [] => {
            // A guessed path is usually nearly right: the name is close to a
            // real one, or the directory exists. A few near names and where to
            // look cost a line each; the whole directory would cost a page for
            // one wrong word.
            let mut out = format!("No indexed file or directory matches `{asked}`.\n");
            let mut parent = asked;
            let mut holds = None;
            while let Some((above, _)) = parent.rsplit_once('/') {
                let prefix = format!("{above}/");
                let count = index.files.iter().filter(|file| file.path.starts_with(&prefix)).count();
                if count > 0 {
                    holds = Some((above, prefix, count));
                    break;
                }
                parent = above;
            }
            // Only the directory that was named vouches for loose matches: one
            // found further up holds half the repository.
            let named = asked.rsplit_once('/').map(|(directory, _)| directory);
            let beside = holds
                .as_ref()
                .filter(|(above, _, _)| Some(*above) == named)
                .map(|(_, prefix, _)| prefix.as_str());
            let near = near_names(index, asked, beside);
            if !near.is_empty() {
                out.push_str("Closest names:\n");
                for file in near {
                    let _ = writeln!(out, "  {}", index.files[file].path);
                }
            }
            if let Some((above, _, count)) = holds {
                let _ = writeln!(out, "`{above}` exists and holds {count} files; outline it for the list.");
            }
            out
        }
        [file] => file_outline(index, *file),
        many => {
            let mut out = format!("`{asked}` matches {} files; ask for one:\n", many.len());
            for &file in many.iter().take(MAX_CANDIDATES) {
                let _ = writeln!(out, "  {}", index.files[file].path);
            }
            if many.len() > MAX_CANDIDATES {
                let _ = writeln!(out, "  ... and {} more", many.len() - MAX_CANDIDATES);
            }
            out
        }
    }
}

/// A run of custom properties at least this long is one outline entry.
const FOLDED_PROPERTIES: usize = 6;
/// A stylesheet declaring more rules than this is outlined by block.
const GROUPED_RULES: usize = 120;

/// The block a stylesheet name belongs to: `table__row`, `table--wide`,
/// `table_row` and `table` are all `table`; `--gap` and `$gap` are variables.
fn block_of(name: &str) -> &str {
    if name.starts_with("--") || name.starts_with('$') {
        return "variables";
    }
    let end = ["__", "--", "_"]
        .iter()
        .filter_map(|mark| name.find(mark))
        .min()
        .unwrap_or(name.len());
    &name[..end]
}
/// Names shown for a path that matched nothing.
const NEAR_NAMES: usize = 5;
/// Shared trigrams, as a share of both names, below which a name is not near.
const NEAR_ENOUGH: f32 = 0.3;
const NEAR_ELSEWHERE: f32 = 0.6;

/// The files whose names are most like the one asked for, those under
/// `beside` first among equals: a wrong name in the right directory is the
/// common mistake, the right name in a wrong directory the next.
fn near_names(index: &Index, asked: &str, beside: Option<&str>) -> Vec<usize> {
    let stem = |path: &str| {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.split('.').next().unwrap_or(name).to_lowercase()
    };
    let trigrams = |name: &str| -> Vec<[char; 3]> {
        let padded: Vec<char> = format!(" {name} ").chars().collect();
        padded.windows(3).map(|three| [three[0], three[1], three[2]]).collect()
    };
    let wanted = trigrams(&stem(asked));
    let mut scored: Vec<(f32, usize)> = index
        .files
        .iter()
        .enumerate()
        .filter_map(|(file, entry)| {
            let has = trigrams(&stem(&entry.path));
            let shared = wanted.iter().filter(|three| has.contains(three)).count();
            let likeness = 2.0 * shared as f32 / (wanted.len() + has.len()) as f32;
            // In the directory that was named a loose likeness will do, and
            // comes first; elsewhere only a name that is nearly the same.
            let beside = beside.is_some_and(|prefix| entry.path.starts_with(prefix));
            let enough = if beside { NEAR_ENOUGH } else { NEAR_ELSEWHERE };
            // A test is named after what it tests and would take its place twice.
            let place = if beside { 1.0 } else { 0.0 } - if entry.kind == Kind::Test { 0.5 } else { 0.0 };
            (likeness >= enough).then_some((likeness + place, file))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(NEAR_NAMES).map(|(_, file)| file).collect()
}

fn file_outline(index: &Index, file: usize) -> String {
    let entry = &index.files[file];
    let text = std::fs::read_to_string(index.root.join(&entry.path)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let mut out = format!("{}{}  ({} lines{})\n", index.label, entry.path, lines.len(), kind_note(entry.kind));

    // Every declaration once, in file order: chunks cut as windows overlap
    // and would list what lies in both twice.
    let mut declared: Vec<(u32, &str)> = index
        .chunks
        .iter()
        .filter(|chunk| chunk.file as usize == file)
        .flat_map(|chunk| chunk.name_lines.iter().copied().zip(chunk.names.iter().map(String::as_str)))
        .collect();
    declared.sort_unstable();
    declared.dedup_by_key(|(line, _)| *line);

    // A stylesheet of a whole application declares hundreds of rules, most
    // of them one line each, and listing all of them would cost more than
    // reading the file. They are grouped by the block they belong to --
    // `.table`, `.table__row`, `.table--wide` are one block -- with where the
    // block's rules lie, and a block is opened with `path`.
    let sheet = entry.path.rsplit('.').next().is_some_and(|ext| matches!(ext, "css" | "scss" | "less" | "pcss" | "postcss"));
    if sheet && declared.len() > GROUPED_RULES {
        let mut blocks: Vec<(String, u32, u32, usize)> = Vec::new();
        for &(line, name) in &declared {
            let block = block_of(name).to_owned();
            match blocks.last_mut() {
                Some((last, _, end, count)) if *last == block => {
                    *end = line;
                    *count += 1;
                }
                _ => blocks.push((block, line, line, 1)),
            }
        }
        let _ = writeln!(out, "  {} rules in {} blocks; `search` a name, or `usages` a class, for its rule:", declared.len(), blocks.len());
        for (block, start, end, count) in &blocks {
            let span = if start == end { format!("{start}") } else { format!("{start}-{end}") };
            let _ = writeln!(out, "{span:>11}  {block}  ({count})");
        }
        return out;
    }

    let mut at = 0;
    while at < declared.len() {
        let (line, name) = declared[at];
        let Some(source) = lines.get(line as usize - 1) else {
            at += 1;
            continue;
        };
        // Nesting is kept, so a method reads as its class's.
        let columns: usize = source
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum();
        let depth = columns.div_ceil(4).min(3);
        let indent = "  ".repeat(depth);
        // A theme sets custom properties by the hundred, one a line; they are
        // one entry here, as are a sheet of `$variables`; `search` or `usages`
        // answers about any one of them.
        let run = declared[at..].iter().take_while(|(_, name)| name.starts_with("--") || name.starts_with('$')).count();
        if run >= FOLDED_PROPERTIES {
            let (last, _) = declared[at + run - 1];
            let _ = writeln!(out, "{line:>6}  {indent}{name} ... {run} variables, to line {last}");
            at += run;
            continue;
        }
        let signature = source.trim();
        // A rule written out on one line is its selector here, not its body.
        let signature = if sheet { signature.split('{').next().unwrap_or(signature) } else { signature };
        let signature = signature.trim_end_matches(['{', '(', ':']).trim_end();
        // A nested rule is written as `&__title` and known as `card__title`.
        let known_as = if signature.starts_with('&') { format!("  = {name}") } else { String::new() };
        let _ = writeln!(out, "{line:>6}  {indent}{}{known_as}", clip(signature));
        at += 1;
    }
    if declared.is_empty() {
        out.push_str("  (no declarations recognised; read the file, or search within it with `path`)\n");
    }
    out
}

fn directory(index: &Index, prefix: &str) -> String {
    // Immediate files with what they declare; deeper ones counted by directory.
    let mut files: Vec<(usize, &str)> = Vec::new();
    let mut below: BTreeMap<&str, usize> = BTreeMap::new();
    for (file, entry) in index.files.iter().enumerate() {
        let Some(rest) = entry.path.strip_prefix(prefix) else {
            continue;
        };
        match rest.split_once('/') {
            Some((directory, _)) => *below.entry(directory).or_default() += 1,
            None => files.push((file, rest)),
        }
    }
    let shown = if prefix.is_empty() && index.label.is_empty() { "./".to_owned() } else { format!("{}{prefix}", index.label) };
    let mut out = format!("{shown}  ({} files here, {} directories)\n", files.len(), below.len());
    for (directory, count) in below.iter().take(MAX_ENTRIES) {
        let _ = writeln!(out, "  {directory}/  ({count} files)");
    }
    for &(file, name) in files.iter().take(MAX_ENTRIES.saturating_sub(below.len())) {
        let chunks = index.chunks.iter().filter(|chunk| chunk.file as usize == file);
        let length = chunks.clone().map(|chunk| chunk.end_line).max().unwrap_or(0);
        // A document is named by its title; its sections are its own business.
        let shown = if index.files[file].kind == Kind::Docs { 1 } else { NAMES_PER_FILE };
        let names: Vec<&str> = chunks
            .flat_map(|chunk| chunk.names.iter().map(String::as_str))
            .take(shown + 1)
            .collect();
        let more = if names.len() > shown { ", ..." } else { "" };
        let listed = names[..names.len().min(shown)].join(", ");
        let _ = writeln!(out, "  {name}  ({length} lines)  {listed}{more}");
    }
    if files.len() + below.len() > MAX_ENTRIES {
        let _ = writeln!(out, "  ... {} entries not shown; ask for a subdirectory", files.len() + below.len() - MAX_ENTRIES);
    }
    out
}

fn kind_note(kind: Kind) -> &'static str {
    match kind {
        Kind::Code => "",
        Kind::Test => ", tests or fixtures",
        Kind::Docs => ", docs",
        Kind::Config => ", config",
    }
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_SIGNATURE_CHARS {
        return line.to_owned();
    }
    let mut clipped: String = line.chars().take(MAX_SIGNATURE_CHARS).collect();
    clipped.push('…');
    clipped
}
