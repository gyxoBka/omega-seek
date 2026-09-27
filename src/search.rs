use crate::index::{Index, Kind};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Content {
    Code,
    Docs,
    Config,
    All,
}

impl Content {
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "code" => Some(Self::Code),
            "docs" => Some(Self::Docs),
            "config" => Some(Self::Config),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn admits(self, kind: Kind) -> bool {
        match self {
            Self::Code => matches!(kind, Kind::Code | Kind::Test),
            Self::Docs => kind == Kind::Docs,
            Self::Config => kind == Kind::Config,
            Self::All => true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub limit: usize,
    pub content: Content,
    pub path: Option<String>,
    pub snippet_lines: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limit: 8,
            content: Content::Code,
            path: None,
            snippet_lines: 12,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub chunk: usize,
    pub score: f32,
    pub start_line: u32,
    pub end_line: u32,
    pub coverage: f32,
    pub declares: bool,
}

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;
const CHANNEL_DEPTH: usize = 100;
const RRF_K: f32 = 60.0;
const NAME_BOOST: f32 = 2.0 / (RRF_K + 1.0);
const FILE_WEIGHT: f32 = 1.0;
const FILE_DENSE_WEIGHT: f32 = 0.5;
const TEST_WEIGHT: f32 = 0.6;
const IMPORTS_WEIGHT: f32 = 0.5;
const HEADER_WEIGHT: f32 = 0.8;
const PREAMBLE_YIELD: f32 = 0.7;
const SHOWN_NAMES: usize = 8;
const FULL_SNIPPETS: usize = 3;
const SKIM_LINES: usize = 4;
const DECLARED_GAP: f32 = 0.75;
const TEMPLATE_HEADING: usize = 4;
const DECLARED_COMPANY: usize = 3;
const CONFIDENT_COVERAGE: f32 = 0.5;
const TAIL_COVERAGE: f32 = 0.6;
const DECLARATION_LINES: usize = 80;
const DOC_LINES: usize = 15;
const JOIN_DEPTH: usize = 40;
const JOIN_MAX_LINES: u32 = 120;
const SAME_FILE_DECAY: f32 = 0.85;

#[must_use]
pub fn search(index: &Index, query: &str, options: &Options) -> Vec<Hit> {
    let admitted: Vec<bool> = index
        .chunks
        .iter()
        .map(|chunk| {
            let file = &index.files[chunk.file as usize];
            options.content.admits(file.kind)
                && options
                    .path
                    .as_deref()
                    .is_none_or(|needle| file.path.contains(needle))
        })
        .collect();

    let terms = index.tokenizer.query_terms(query);
    let mut fused = vec![0.0f32; index.chunks.len()];
    let asked = query_vector(index, query);
    let dense = asked
        .as_deref()
        .map(|asked| top(&index.chunk_scores(asked), &admitted))
        .unwrap_or_default();
    let (lexical, coverage) = lexical(index, &terms, &admitted);
    for ranking in [lexical, dense] {
        for (rank, chunk) in ranking.into_iter().enumerate() {
            fused[chunk] += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
    }
    let every_file = vec![true; index.files.len()];
    let mut file_rankings = vec![file_ranks(index, &terms)];
    if let Some(asked) = asked.as_deref().filter(|_| query.split_whitespace().count() > 1) {
        let scores = index.file_scores(asked);
        file_rankings.push(ranks_of(&top(&scores, &every_file), index.files.len()));
    }
    for (file_rank, weight) in file_rankings.iter().zip([FILE_WEIGHT, FILE_DENSE_WEIGHT]) {
        for (id, chunk) in index.chunks.iter().enumerate() {
            if fused[id] > 0.0 {
                if let Some(rank) = file_rank[chunk.file as usize] {
                    fused[id] += weight / (RRF_K + rank as f32 + 1.0);
                }
            }
        }
    }

    let raw: Vec<&str> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| word.chars().count() >= 3)
        .collect();
    let alone = raw.len() == 1;
    let words: Vec<String> = raw
        .into_iter()
        .filter(|word| alone || word.contains('_') || word.chars().skip(1).any(char::is_uppercase))
        .map(str::to_lowercase)
        .chain(hyphenated(query).map(str::to_lowercase))
        .collect();
    let mut heading_files: std::collections::HashMap<&str, std::collections::BTreeSet<u32>> = std::collections::HashMap::new();
    let mut candidates: Vec<(u32, u32)> = plain_words(query)
        .iter()
        .filter_map(|word| index.headings().get(word))
        .flatten()
        .copied()
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    for (chunk, name) in candidates {
        let chunk = &index.chunks[chunk as usize];
        let name = &chunk.names[name as usize];
        if heads(name, query) {
            heading_files.entry(name.as_str()).or_default().insert(chunk.file);
        }
    }
    let titled = |name: &str| heading_files.get(name).is_some_and(|files| files.len() < TEMPLATE_HEADING);
    let mut hits: Vec<Hit> = Vec::new();
    for (id, chunk) in index.chunks.iter().enumerate() {
        if !admitted[id] {
            continue;
        }
        let mut score = fused[id];
        let declares = chunk.names.iter().any(|name| {
            words.iter().any(|word| name.eq_ignore_ascii_case(word))
                || (index.files[chunk.file as usize].kind == Kind::Docs && titled(name))
        });
        if declares {
            score += NAME_BOOST;
        }
        if score <= 0.0 {
            continue;
        }
        if index.files[chunk.file as usize].kind == Kind::Test {
            score *= TEST_WEIGHT;
        }
        let path = &index.files[chunk.file as usize].path;
        if !declares && [".h", ".hpp", ".hh", ".hxx"].iter().any(|ending| path.ends_with(ending)) {
            score *= HEADER_WEIGHT;
        }
        if chunk.imports && !declares {
            score *= IMPORTS_WEIGHT;
        }
        hits.push(Hit {
            chunk: id,
            score,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            coverage: coverage[id],
            declares,
        });
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk.cmp(&b.chunk)));
    yield_to_the_body(index, &mut hits);
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk.cmp(&b.chunk)));
    join_neighbours(index, &mut hits);

    let mut per_file: std::collections::HashMap<u32, i32> = std::collections::HashMap::new();
    for hit in &mut hits {
        let seen = per_file.entry(index.chunks[hit.chunk].file).or_insert(0);
        hit.score *= SAME_FILE_DECAY.powi(*seen);
        *seen += 1;
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk.cmp(&b.chunk)));
    hits.truncate(options.limit);
    hits
}

fn yield_to_the_body(index: &Index, hits: &mut [Hit]) {
    let depth = hits.len().min(JOIN_DEPTH);
    for opening in 0..depth {
        let chunk = &index.chunks[hits[opening].chunk];
        let preamble = chunk.start_line == 1
            && !hits[opening].declares
            && chunk.names.iter().all(|name| !name.chars().any(char::is_lowercase))
            && index.file_lengths[chunk.file as usize] > chunk.length * 2.0;
        if !preamble {
            continue;
        }
        let floor = hits[opening].score * PREAMBLE_YIELD;
        let body = (opening + 1..depth).find(|&other| {
            index.chunks[hits[other].chunk].file == chunk.file && hits[other].score >= floor
        });
        if let Some(body) = body {
            let (above, below) = (hits[opening].score, hits[body].score);
            hits[opening].score = below;
            hits[body].score = above;
        }
    }
}

fn join_neighbours(index: &Index, hits: &mut Vec<Hit>) {
    let depth = hits.len().min(JOIN_DEPTH);
    let mut absorbed = vec![false; depth];
    for keep in 0..depth {
        if absorbed[keep] {
            continue;
        }
        let mut grown = true;
        while grown {
            grown = false;
            for other in keep + 1..depth {
                if absorbed[other] {
                    continue;
                }
                let (a, b) = (hits[keep], hits[other]);
                let same_file = index.chunks[a.chunk].file == index.chunks[b.chunk].file;
                let touching = b.start_line <= a.end_line + 2 && a.start_line <= b.end_line + 2;
                let start = a.start_line.min(b.start_line);
                let end = a.end_line.max(b.end_line);
                if same_file && touching && end - start < JOIN_MAX_LINES {
                    hits[keep].start_line = start;
                    hits[keep].end_line = end;
                    hits[keep].coverage = a.coverage.max(b.coverage);
                    hits[keep].declares = a.declares || b.declares;
                    absorbed[other] = true;
                    grown = true;
                }
            }
        }
    }
    let mut position = 0;
    hits.retain(|_| {
        position += 1;
        position > depth || !absorbed[position - 1]
    });
}

fn lexical(index: &Index, terms: &[String], admitted: &[bool]) -> (Vec<usize>, Vec<f32>) {
    let lengths: Vec<f32> = index.chunks.iter().map(|chunk| chunk.length).collect();
    let (scores, coverage) = bm25(|term| index.postings(term), &lengths, index.average_length, terms);
    (top(&scores, admitted), coverage)
}

fn file_ranks(index: &Index, terms: &[String]) -> Vec<Option<usize>> {
    let (scores, _) = bm25(
        |term| index.file_postings(term),
        &index.file_lengths,
        index.average_file_length,
        terms,
    );
    ranks_of(&top(&scores, &vec![true; scores.len()]), scores.len())
}

fn ranks_of(ranking: &[usize], count: usize) -> Vec<Option<usize>> {
    let mut ranks = vec![None; count];
    for (rank, &document) in ranking.iter().enumerate() {
        ranks[document] = Some(rank);
    }
    ranks
}

fn bm25(
    postings: impl Fn(&str) -> Vec<(u32, f32)>,
    lengths: &[f32],
    average_length: f32,
    terms: &[String],
) -> (Vec<f32>, Vec<f32>) {
    let count = lengths.len() as f32;
    let mut scores = vec![0.0f32; lengths.len()];
    let mut matched = vec![0u16; lengths.len()];
    for term in terms {
        let postings = postings(term);
        if postings.is_empty() {
            continue;
        }
        let frequency = postings.len() as f32;
        let idf = (1.0 + (count - frequency + 0.5) / (frequency + 0.5)).ln();
        for &(document, tf) in &postings {
            let length = lengths[document as usize] / average_length.max(1.0);
            let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * length);
            scores[document as usize] += idf * tf * (BM25_K1 + 1.0) / norm;
            matched[document as usize] += 1;
        }
    }
    let total = terms.len().max(1) as f32;
    let coverage: Vec<f32> = matched.iter().map(|&matched| f32::from(matched) / total).collect();
    for (score, coverage) in scores.iter_mut().zip(&coverage) {
        *score *= coverage;
    }
    (scores, coverage)
}

fn query_vector(index: &Index, query: &str) -> Option<Vec<f32>> {
    let dimension = index.dense()?;
    let vector = index.model.as_ref()?.encode(&[query.to_owned()]).into_iter().next()?;
    (dimension > 0 && vector.len() == dimension).then_some(vector)
}

fn top(scores: &[f32], admitted: &[bool]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len())
        .filter(|&id| admitted[id] && scores[id] > 0.0)
        .collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order.truncate(CHANNEL_DEPTH);
    order
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Detail {
    Declaration,
    Snippet,
    Skim,
    Heading,
}

#[must_use]
pub fn present(hits: &[Hit]) -> (Vec<(Hit, Detail)>, bool) {
    let Some(first) = hits.first() else {
        return (Vec::new(), true);
    };
    let runner_up = hits.get(1).map_or(0.0, |hit| hit.score / first.score);
    if first.declares && runner_up < DECLARED_GAP {
        let mut shown = vec![(*first, Detail::Declaration)];
        shown.extend(
            hits.iter()
                .skip(1)
                .filter(|hit| hit.declares)
                .take(DECLARED_COMPANY)
                .map(|hit| (*hit, Detail::Skim)),
        );
        return (shown, true);
    }
    let best = hits
        .iter()
        .take(FULL_SNIPPETS)
        .map(|hit| hit.coverage)
        .fold(0.0, f32::max);
    let confident = best >= CONFIDENT_COVERAGE || first.declares;
    let shown = hits
        .iter()
        .enumerate()
        .map(|(place, hit)| {
            let detail = if place < FULL_SNIPPETS {
                if confident { Detail::Snippet } else { Detail::Skim }
            } else if hit.coverage + f32::EPSILON >= TAIL_COVERAGE * best {
                Detail::Skim
            } else {
                Detail::Heading
            };
            (*hit, detail)
        })
        .collect();
    (shown, confident)
}

#[must_use]
pub fn documented(index: &Index, query: &str, options: &Options, hits: &[Hit]) -> Option<(Vec<Hit>, &'static str)> {
    if options.content != Content::Code || hits.first().is_some_and(|hit| hit.declares) {
        return None;
    }
    let in_docs = Options { content: Content::Docs, ..options.clone() };
    let documented = search(index, query, &in_docs);
    documented
        .first()
        .is_some_and(|hit| hit.declares)
        .then_some((documented, "A document has a section by that name"))
}

#[must_use]
pub fn render(index: &Index, query: &str, hits: &[Hit], options: &Options) -> String {
    if index.files.is_empty() {
        return nothing_indexed(index);
    }
    if let Some((documented, why)) = documented(index, query, options, hits) {
        let in_docs = Options { content: Content::Docs, ..options.clone() };
        let mut out = format!("{why}:

");
        out.push_str(&render(index, query, &documented, &in_docs));
        return out;
    }
    if hits.is_empty() {
        if let Some(path) = &options.path {
            let anywhere = Options { path: None, ..options.clone() };
            let outside = search(index, query, &anywhere);
            if let Some(first) = outside.first() {
                let file = &index.files[index.chunks[first.chunk].file as usize];
                return format!(
                    "No matches under `{path}`, but there are matches elsewhere, first in {}{}. Drop or change `path`.",
                    index.label, file.path
                );
            }
        }
        return "No matches. Try the words the code itself would use.".to_owned();
    }
    let terms = index.tokenizer.query_terms(query);
    let (mut shown, confident) = present(hits);
    let mut out = String::new();
    let name = query.trim();
    let lone_name = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    let titled_in_docs = || {
        let in_docs = Options { content: Content::Docs, ..options.clone() };
        search(index, name, &in_docs).first().is_some_and(|hit| hit.declares)
    };
    if lone_name && !hits[0].declares && crate::usages::count(index, name).1 == 0 && !titled_in_docs() {
        shown.retain(|(hit, _)| hit.coverage + f32::EPSILON >= 1.0);
        shown.truncate(FULL_SNIPPETS);
        for (_, detail) in &mut shown {
            *detail = Detail::Skim;
        }
        if shown.is_empty() {
            return format!(
                "Nothing in the code is named `{name}` (whole word, case-sensitive), and the words it is \
                 made of occur together nowhere. Check the spelling, or search by what it does{}.",
                if options.content == Content::Code { "; `content: \"all\"` also looks in docs and config" } else { "" }
            );
        }
        let _ = write!(
            out,
            "Nothing in the code is named `{name}` (whole word, case-sensitive). Its words occur together in:\n\n"
        );
    } else if !confident {
        out.push_str(
            "Low confidence: few of the query's words occur together anywhere. Rephrase with the \
             technical terms the code would use before trusting or reading these.\n\n",
        );
    }
    for (hit, detail) in shown {
        let chunk = &index.chunks[hit.chunk];
        let file = &index.files[chunk.file as usize];
        let text = match detail {
            Detail::Heading => None,
            _ if options.snippet_lines == 0 => None,
            _ => std::fs::read_to_string(index.root.join(&file.path)).ok(),
        };
        let all: Vec<&str> = text.as_deref().map(|text| text.lines().collect()).unwrap_or_default();

        let declared = (detail == Detail::Declaration && !all.is_empty())
            .then(|| declared_line(index, &hit, query))
            .flatten()
            .map(|(_, line)| {
                if file.kind == Kind::Docs {
                    section_extent(&all, line as usize)
                } else {
                    declaration_extent(&all, line as usize)
                }
            });
        let (start, end) = declared.unwrap_or((hit.start_line as usize, hit.end_line as usize));

        let _ = write!(out, "{}{}:{start}-{end}", index.label, file.path);
        let names: Vec<&str> = index
            .chunks
            .iter()
            .filter(|other| {
                other.file == chunk.file
                    && other.start_line >= hit.start_line
                    && other.end_line <= hit.end_line
            })
            .flat_map(|other| other.names.iter().map(String::as_str))
            .take(SHOWN_NAMES)
            .collect();
        if declared.is_none() && !names.is_empty() {
            let _ = write!(out, "  {}", names.join(", "));
        }
        out.push('\n');
        if all.is_empty() {
            continue;
        }
        let lines = &all[(start - 1).min(all.len())..end.min(all.len())];
        let (first, count) = match detail {
            Detail::Declaration if declared.is_some() => (0, DECLARATION_LINES),
            Detail::Snippet | Detail::Declaration => {
                (best_window(index, &terms, lines, options.snippet_lines), options.snippet_lines)
            }
            _ => {
                let count = options.snippet_lines.min(SKIM_LINES);
                (best_window(index, &terms, lines, count), count)
            }
        };
        for (offset, line) in lines.iter().enumerate().skip(first).take(count) {
            let _ = writeln!(out, "{:>5}| {}", start + offset, line.trim_end());
        }
        if declared.is_none() && lines.len() > count {
            let _ = writeln!(out, "       ... {count} of {} lines shown", lines.len());
        }
        if declared.is_some() {
            if lines.len() > count {
                let _ = writeln!(out, "       ... {} more lines, to {end}", lines.len() - count);
            }
            if let Some((name, _)) = declared_line(index, &hit, query).filter(|_| file.kind != Kind::Docs) {
                let (others, files) = crate::usages::count(index, name);
                if others == 0 {
                    let _ = writeln!(out, "       not named anywhere else in the code");
                } else {
                let _ = writeln!(
                    out,
                    "       named on {others} other line{} across {files} file{} (`usages` lists them)",
                    if others == 1 { "" } else { "s" },
                    if files == 1 { "" } else { "s" },
                );
                }
            }
        }
        out.push('\n');
    }
    out
}

#[must_use]
pub fn nothing_indexed(index: &Index) -> String {
    format!(
        "Nothing is indexed under {}: no source files were found there. This is almost certainly \
         the wrong directory, not an empty project -- start omega from the repository root \
         or pass --root, and fall back to grep for now.",
        index.root.display()
    )
}

pub(crate) fn plain_words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| word.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

fn heads(heading: &str, query: &str) -> bool {
    let heading = plain_words(heading);
    let query = plain_words(query);
    if heading.is_empty() || query.is_empty() {
        return false;
    }
    let said = query.iter().filter(|word| heading.contains(word)).count();
    let names_it = heading.iter().all(|word| query.contains(word)) && heading.len() * 2 > query.len();
    let opens_with = said == query.len() && said * 2 >= heading.len();
    let keyed = query.len() == 1 && heading[0] == query[0] && query[0].chars().any(char::is_numeric);
    keyed || ((heading.len() >= 2 || heading[0].chars().count() >= 5) && (names_it || opens_with))
}

fn hyphenated(query: &str) -> impl Iterator<Item = &str> {
    query
        .split_whitespace()
        .map(|word| word.trim_start_matches(['.', '#', '%']).trim_end_matches([',', ':', ';', '{']))
        .filter(|word| word.contains('-') && word.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '$')))
}

fn declared_line<'a>(index: &'a Index, hit: &Hit, query: &str) -> Option<(&'a str, u32)> {
    let words: Vec<&str> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .filter(|word| !word.is_empty())
        .chain(hyphenated(query))
        .collect();
    let file = index.chunks[hit.chunk].file;
    let documented = index.files[file as usize].kind == Kind::Docs;
    index
        .chunks
        .iter()
        .filter(|chunk| chunk.file == file && chunk.start_line >= hit.start_line && chunk.end_line <= hit.end_line)
        .flat_map(|chunk| chunk.names.iter().zip(&chunk.name_lines))
        .find(|(name, _)| words.iter().any(|word| name.eq_ignore_ascii_case(word)) || (documented && heads(name, query)))
        .map(|(name, &line)| (name.as_str(), line))
}

fn section_extent(lines: &[&str], heading: usize) -> (usize, usize) {
    let level = |line: &str| line.chars().take_while(|&c| c == '#').count();
    let opened = level(lines[heading - 1]);
    let mut fenced = false;
    let mut end = lines.len();
    for (offset, line) in lines.iter().enumerate().skip(heading) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        let depth = level(trimmed);
        if !fenced && depth > 0 && depth <= opened && trimmed[depth..].starts_with(' ') {
            end = offset;
            break;
        }
    }
    while end > heading && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    (heading, end)
}

fn declaration_extent(lines: &[&str], declared: usize) -> (usize, usize) {
    let indent = |line: &str| line.len() - line.trim_start().len();
    let at = declared - 1;
    let depth = indent(lines[at]);
    if lines[at].trim_start().starts_with('}') {
        let opener = (0..at)
            .rev()
            .find(|&line| !lines[line].trim().is_empty() && indent(lines[line]) <= depth)
            .unwrap_or(at);
        return (opener + 1, declared);
    }

    let mut start = at;
    while start > 0 && at - start < DOC_LINES {
        let above = lines[start - 1].trim_start();
        let documents = ["///", "//", "/**", "*", "#[", "@", "#"].iter().any(|mark| above.starts_with(mark));
        if !documents || indent(lines[start - 1]) != depth && !above.starts_with('*') {
            break;
        }
        start -= 1;
    }

    let mut end = at;
    for (offset, line) in lines.iter().enumerate().skip(at + 1) {
        let body = line.trim();
        if body.is_empty() {
            continue;
        }
        if indent(line) > depth {
            end = offset;
            continue;
        }
        let closes = body.starts_with(['}', ')', ']']) || body == "end";
        let opens = body.ends_with(['{', '(', '[', ':']) || body.ends_with("=>");
        if closes && opens {
            end = offset;
            continue;
        }
        if closes {
            end = offset;
        }
        break;
    }
    (start + 1, end + 1)
}

fn best_window(index: &Index, terms: &[String], lines: &[&str], size: usize) -> usize {
    if lines.len() <= size {
        return 0;
    }
    let mut buffer = Vec::new();
    let spoken: Vec<u64> = lines
        .iter()
        .map(|line| {
            buffer.clear();
            index.tokenizer.terms(line, &mut buffer);
            terms
                .iter()
                .take(64)
                .enumerate()
                .filter(|(_, term)| buffer.contains(term))
                .fold(0u64, |bits, (bit, _)| bits | 1 << bit)
        })
        .collect();
    let worth = |start: usize| {
        let window = &spoken[start..start + size];
        let distinct = window.iter().fold(0u64, |bits, line| bits | line).count_ones();
        let total: u32 = window.iter().map(|line| line.count_ones()).sum();
        (distinct, total)
    };
    let (mut best, mut best_worth) = (0, worth(0));
    for start in 1..=lines.len() - size {
        let worth = worth(start);
        if worth > best_worth {
            (best, best_worth) = (start, worth);
        }
    }
    if let Some(first) = (best..best + size).find(|&line| spoken[line] != 0) {
        let later = first.saturating_sub(2).min(lines.len() - size);
        if worth(later) >= best_worth {
            return later;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(score: f32, coverage: f32, declares: bool) -> Hit {
        Hit {
            chunk: 0,
            score,
            start_line: 1,
            end_line: 10,
            coverage,
            declares,
        }
    }

    #[test]
    fn a_declaration_found_by_name_needs_no_company() {
        let hits = [hit(1.0, 1.0, true), hit(0.57, 0.5, false), hit(0.5, 0.5, false), hit(0.4, 0.2, false)];
        let (shown, confident) = present(&hits);
        let details: Vec<Detail> = shown.iter().map(|(_, detail)| *detail).collect();
        assert_eq!(details, [Detail::Declaration], "a loose match is no company for a declaration");

        let hits = [hit(1.0, 1.0, true), hit(0.6, 0.5, false), hit(0.5, 1.0, true)];
        let details: Vec<Detail> = present(&hits).0.iter().map(|(_, detail)| *detail).collect();
        assert_eq!(details, [Detail::Declaration, Detail::Skim]);
        assert!(confident);
    }

    #[test]
    fn a_weak_tail_keeps_its_headings_and_loses_its_lines() {
        let hits = [
            hit(1.0, 0.8, false),
            hit(0.9, 0.6, false),
            hit(0.8, 0.6, false),
            hit(0.7, 0.6, false),
            hit(0.6, 0.2, false),
        ];
        let (shown, confident) = present(&hits);
        let details: Vec<Detail> = shown.iter().map(|(_, detail)| *detail).collect();
        assert_eq!(details, [Detail::Snippet, Detail::Snippet, Detail::Snippet, Detail::Skim, Detail::Heading]);
        assert!(confident);
    }

    #[test]
    fn words_that_occur_nowhere_together_are_said_to() {
        let hits = [hit(1.0, 0.3, false), hit(0.95, 0.2, false)];
        let (shown, confident) = present(&hits);
        assert!(!confident);
        assert!(shown.iter().all(|(_, detail)| *detail == Detail::Skim));
    }

    #[test]
    fn a_declaration_runs_from_its_comment_to_its_closing_line() {
        let source = "\
use x;

/// Checks the origin.
#[inline]
fn check(
    request: &Request,
) -> Result<(), Refusal> {
    if request.foreign() {
        return Err(Refusal);
    }

    Ok(())
}

fn next() {}
";
        let lines: Vec<&str> = source.lines().collect();
        assert_eq!(declaration_extent(&lines, 5), (3, 13));
        assert_eq!(declaration_extent(&lines, 15), (15, 15));

        let typedef = ["", "typedef struct {", "    int64_t src;", "    int64_t dst;", "} edge_t;", ""];
        assert_eq!(declaration_extent(&typedef, 5), (2, 5));

        let python = "class A:\n    def run(self):\n        work()\n\n        more()\n\n    def stop(self):\n        pass\n";
        let lines: Vec<&str> = python.lines().collect();
        assert_eq!(declaration_extent(&lines, 2), (2, 5));
    }
}
