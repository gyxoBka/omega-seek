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
        [] => format!("No indexed file or directory matches `{asked}`."),
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

fn file_outline(index: &Index, file: usize) -> String {
    let entry = &index.files[file];
    let text = std::fs::read_to_string(index.root.join(&entry.path)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let mut out = format!("{}{}  ({} lines{})\n", index.label, entry.path, lines.len(), kind_note(entry.kind));

    let mut declared = 0;
    for chunk in index.chunks.iter().filter(|chunk| chunk.file as usize == file) {
        for &line in &chunk.name_lines {
            let Some(source) = lines.get(line as usize - 1) else {
                continue;
            };
            // Nesting is kept, so a method reads as its class's.
            let columns: usize = source
                .chars()
                .take_while(|c| c.is_whitespace())
                .map(|c| if c == '\t' { 4 } else { 1 })
                .sum();
            let depth = columns.div_ceil(4).min(3);
            let signature = source.trim().trim_end_matches(['{', '(', ':']).trim_end();
            let _ = writeln!(out, "{line:>6}  {}{}", "  ".repeat(depth), clip(signature));
            declared += 1;
        }
    }
    if declared == 0 {
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
        let names: Vec<&str> = chunks
            .flat_map(|chunk| chunk.names.iter().map(String::as_str))
            .take(NAMES_PER_FILE + 1)
            .collect();
        let more = if names.len() > NAMES_PER_FILE { ", ..." } else { "" };
        let listed = names[..names.len().min(NAMES_PER_FILE)].join(", ");
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
