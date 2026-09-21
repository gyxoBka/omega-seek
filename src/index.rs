use crate::chunk::chunk;
use crate::tokenize::Tokenizer;
use model2vec_rs::model::StaticModel;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What a file is for, which decides whether a search looks at it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Kind {
    Code,
    Test,
    Docs,
    Config,
}

#[derive(Debug)]
pub struct Chunk {
    pub file: u32,
    /// One-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    /// What the chunk declares, in source order, and the line of each.
    pub names: Vec<String>,
    pub name_lines: Vec<u32>,
    /// Body length in terms, for BM25 length normalisation.
    pub length: f32,
    /// Mostly import lines: many words, none of them what the file does.
    pub imports: bool,
}

#[derive(Debug)]
pub struct FileEntry {
    /// Relative to the root, forward slashes.
    pub path: String,
    pub kind: Kind,
}

pub struct Index {
    pub root: PathBuf,
    pub files: Vec<FileEntry>,
    pub chunks: Vec<Chunk>,
    /// term -> (chunk, weighted frequency)
    pub postings: HashMap<String, Vec<(u32, f32)>>,
    pub average_length: f32,
    /// The same, with a whole file as the document.
    pub file_postings: HashMap<String, Vec<(u32, f32)>>,
    pub file_lengths: Vec<f32>,
    pub average_file_length: f32,
    /// Row-major unit vectors, one per chunk; empty without a model.
    pub vectors: Vec<f32>,
    /// One per file: the direction its chunks share.
    pub file_vectors: Vec<f32>,
    pub dimension: usize,
    pub model: Option<StaticModel>,
    pub tokenizer: Tokenizer,
    /// What each file was read as, so a file that has not changed is not read
    /// again. Keyed by relative path.
    cache: HashMap<String, CachedFile>,
    /// Where the cache is kept between runs, when it is.
    store: Option<PathBuf>,
    /// What an answer puts in front of each path: nothing for the directory
    /// the agent is taken to be in, the root itself for anywhere else, so that
    /// a path in an answer can be opened as it stands.
    pub label: String,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("root", &self.root)
            .field("files", &self.files.len())
            .field("chunks", &self.chunks.len())
            .finish_non_exhaustive()
    }
}

/// A name is worth this many body occurrences, and a path word this many.
const NAME_WEIGHT: f32 = 3.0;
const PATH_WEIGHT: f32 = 2.0;
const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_NAMES: usize = 32;
/// What opens a block without declaring anything.
const CONTROL: &[&str] = &[
    "else", "switch", "catch", "do", "try", "with", "await", "throw", "match", "loop", "when",
    "foreach", "elseif", "elif", "unless", "until", "using", "lock", "synchronized", "case",
    "return", "go", "defer", "select", "range", "yield", "delete", "typeof", "not", "and", "or",
    "in", "is", "as", "echo", "print", "assert",
];
const KEYWORDS: &[&str] = &[
    "use", "function", "fn", "func", "def", "type", "struct", "enum", "interface", "static", "const", "class", "return", "new", "if", "for", "while",
    "async", "abstract", "final", "public", "private", "protected", "readonly", "extends",
    "implements", "self", "this", "var", "let", "mut", "pub", "unsafe", "extern", "default",
];

/// What stands for "this file as it was when read".
#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
struct Stamp {
    /// Seconds and nanoseconds since the epoch.
    modified: Option<(u64, u32)>,
    bytes: u64,
}

#[derive(Deserialize, Serialize)]
struct CachedFile {
    stamp: Stamp,
    /// What the path says the file is: what the walk compares against.
    walked: Kind,
    /// What it is treated as, which its contents may have changed.
    kind: Kind,
    /// False for a file that was looked at and left out.
    indexed: bool,
    path_terms: Vec<String>,
    drafts: Vec<Draft>,
}

#[derive(Deserialize, Serialize)]
struct Draft {
    imports: bool,
    start_line: u32,
    end_line: u32,
    names: Vec<String>,
    name_lines: Vec<u32>,
    length: f32,
    terms: Vec<(String, f32)>,
    vector: Vec<f32>,
}

type Walked = Vec<(String, PathBuf, Kind, Stamp)>;

impl Index {
    /// Read every indexable file under `root` and build the channels.
    pub fn build(root: &Path, model_dir: Option<&Path>) -> Result<Self, String> {
        let model = match model_dir {
            Some(dir) => Some(
                StaticModel::from_pretrained(dir, None, Some(true), None)
                    .map_err(|error| format!("cannot load model from {}: {error}", dir.display()))?,
            ),
            None => None,
        };
        let walked = walk(root);
        Ok(assemble(
            root.to_path_buf(),
            model,
            Tokenizer::new(),
            HashMap::new(),
            walked,
            None,
        ))
    }

    /// The same, keeping what each file was read as on disk between runs, so
    /// a later start reads only the files that changed since.
    pub fn open(root: &Path, model_dir: Option<&Path>) -> Result<Self, String> {
        Self::open_in(root, model_dir, cache_dir().as_deref())
    }

    /// `open`, with the caches kept under `cache_dir` instead of the user's.
    pub fn open_in(root: &Path, model_dir: Option<&Path>, cache_dir: Option<&Path>) -> Result<Self, String> {
        let store = cache_dir.and_then(|dir| store_path(dir, root, model_dir));
        // The model, the cache and the tree do not depend on one another, and
        // each is a good part of a start: they are read side by side.
        let (model, previous, walked) = std::thread::scope(|scope| {
            let model = scope.spawn(|| {
                model_dir
                    .map(|dir| {
                        StaticModel::from_pretrained(dir, None, Some(true), None)
                            .map_err(|error| format!("cannot load model from {}: {error}", dir.display()))
                    })
                    .transpose()
            });
            let previous = scope.spawn(|| store.as_deref().and_then(load).unwrap_or_default());
            let walked = walk(root);
            (model.join(), previous.join(), walked)
        });
        let model = model.map_err(|_| "loading the model panicked")??;
        let previous = previous.unwrap_or_default();
        let unchanged = matches(&previous, &walked);
        let index = assemble(root.to_path_buf(), model, Tokenizer::new(), previous, walked, store);
        if !unchanged {
            index.save();
        }
        Ok(index)
    }

    fn save(&self) {
        let Some(store) = &self.store else { return };
        let Ok(bytes) = bincode::serialize(&(STORE_VERSION, &self.cache)) else {
            return;
        };
        // Beside and renamed: a reader never sees half a cache. A cache that
        // cannot be written is only a slower next start.
        let aside = store.with_extension("tmp");
        let _ = store.parent().map(std::fs::create_dir_all);
        if std::fs::write(&aside, bytes).is_ok() {
            let _ = std::fs::rename(&aside, store);
        }
    }

    /// The index of the tree as it is now. Files whose size and modification
    /// time are what they were are not read again; when nothing changed at
    /// all the index is handed back untouched.
    #[must_use]
    pub fn refreshed(self) -> Self {
        let walked = walk(&self.root);
        if matches(&self.cache, &walked) {
            return self;
        }
        let label = self.label;
        let mut index = assemble(self.root, self.model, self.tokenizer, self.cache, walked, self.store);
        index.label = label;
        index.save();
        index
    }

    /// Whether `root` holds more indexable files than `limit`, found without
    /// reading any of them: a root named by mistake is refused before it costs
    /// minutes.
    #[must_use]
    pub fn holds_more_than(root: &Path, limit: usize) -> bool {
        ignore::WalkBuilder::new(root)
            .add_custom_ignore_filename(".omegaignore")
            .require_git(false)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter(|entry| {
                let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
                !is_junk(relative) && kind_of(relative).is_some()
            })
            .nth(limit)
            .is_some()
    }
}

/// Bumped whenever chunking, tokenizing, naming or embedding changes what a
/// file is read as: an older cache is then ignored rather than trusted.
const STORE_VERSION: u32 = 6;

/// Whether the cache describes exactly the tree that was walked.
fn matches(cache: &HashMap<String, CachedFile>, walked: &Walked) -> bool {
    walked.len() == cache.len()
        && walked.iter().all(|(relative, _, kind, stamp)| {
            cache
                .get(relative)
                .is_some_and(|cached| cached.stamp == *stamp && cached.walked == *kind)
        })
}

/// One cache per repository and model: vectors from one model mean nothing
/// to another, and a lexical-only cache has none.
fn cache_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(base)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".cache")
    };
    Some(base.join("omega").join("index"))
}

fn store_path(cache_dir: &Path, root: &Path, model_dir: Option<&Path>) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let model = model_dir.map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    let name: String = hasher.finalize().iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    Some(cache_dir.join(format!("{name}.bin")))
}

fn load(store: &Path) -> Option<HashMap<String, CachedFile>> {
    let bytes = std::fs::read(store).ok()?;
    let (version, cache): (u32, HashMap<String, CachedFile>) = bincode::deserialize(&bytes).ok()?;
    (version == STORE_VERSION).then_some(cache)
}

fn walk(root: &Path) -> Walked {
    let mut walked: Walked = ignore::WalkBuilder::new(root)
        .add_custom_ignore_filename(".omegaignore")
        // A .gitignore says what is not source whether or not the tree has
        // been `git init`ed yet.
        .require_git(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if meta.len() > MAX_FILE_BYTES {
                return None;
            }
            let path = entry.into_path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if is_junk(relative) {
                return None;
            }
            let kind = kind_of(relative)?;
            let stamp = Stamp {
                modified: meta
                    .modified()
                    .ok()
                    .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|since| (since.as_secs(), since.subsec_nanos())),
                bytes: meta.len(),
            };
            Some((relative.to_string_lossy().replace('\\', "/"), path, kind, stamp))
        })
        .collect();
    walked.sort_by(|a, b| a.0.cmp(&b.0));
    walked
}

fn assemble(
    root: PathBuf,
    model: Option<StaticModel>,
    tokenizer: Tokenizer,
    mut previous: HashMap<String, CachedFile>,
    walked: Walked,
    store: Option<PathBuf>,
) -> Index {
    // What is still valid leaves the old cache first, so the parallel stage
    // reads only what it has to.
    let kept: Vec<Option<CachedFile>> = walked
        .iter()
        .map(|(relative, _, kind, stamp)| {
            previous
                .remove(relative)
                .filter(|cached| cached.stamp == *stamp && cached.walked == *kind)
        })
        .collect();
    let read: Vec<Option<CachedFile>> = walked
        .par_iter()
        .zip(kept)
        .map(|((relative, path, kind, stamp), kept)| {
            if kept.is_some() {
                return kept;
            }
            // A file that cannot be read as text, or is not worth reading, is
            // remembered as such: forgetting it would make every refresh see
            // a tree that differs from the cache and rebuild for nothing.
            let text = std::fs::read_to_string(path).ok().filter(|text| !is_minified(text));
            let Some(text) = text else {
                return Some(CachedFile {
                    stamp: *stamp,
                    walked: *kind,
                    kind: *kind,
                    indexed: false,
                    path_terms: Vec::new(),
                    drafts: Vec::new(),
                });
            };
            let mut path_terms = Vec::new();
            let mut tail: Vec<&str> = relative.rsplit('/').take(2).collect();
            tail.reverse();
            tokenizer.terms(&tail.join(" "), &mut path_terms);
            path_terms.sort();
            path_terms.dedup();
            let tests = *kind == Kind::Code && !relative.ends_with(".rs") && reads_as_tests(&text);
            Some(CachedFile {
                stamp: *stamp,
                walked: *kind,
                kind: if tests { Kind::Test } else { *kind },
                indexed: true,
                path_terms,
                drafts: draft_file(&tokenizer, model.as_ref(), &text),
            })
        })
        .collect();

    let dimension = read
        .iter()
        .flatten()
        .flat_map(|cached| cached.drafts.iter().map(|draft| draft.vector.len()))
        .max()
        .unwrap_or(0);
    let mut files = Vec::new();
    let mut chunks = Vec::new();
    let mut vectors = Vec::new();
    let mut file_vectors = Vec::new();
    let mut postings: HashMap<String, Vec<(u32, f32)>> = HashMap::new();
    let mut file_postings: HashMap<String, Vec<(u32, f32)>> = HashMap::new();
    let mut file_lengths = Vec::new();
    let mut total_length = 0.0f32;
    let mut cache = HashMap::with_capacity(walked.len());
    for ((relative, ..), cached) in walked.into_iter().zip(read) {
        let Some(cached) = cached else { continue };
        if !cached.indexed {
            cache.insert(relative, cached);
            continue;
        }
        let file = files.len() as u32;
        files.push(FileEntry {
            path: relative.clone(),
            kind: cached.kind,
        });
        let mut file_weights: HashMap<&str, f32> = HashMap::new();
        for term in &cached.path_terms {
            *file_weights.entry(term).or_default() += PATH_WEIGHT;
        }
        let mut file_length = 0.0f32;
        let mut file_vector = vec![0.0f32; dimension];
        for draft in &cached.drafts {
            let id = chunks.len() as u32;
            for (term, weight) in &draft.terms {
                *file_weights.entry(term).or_default() += weight;
                let in_path = if cached.path_terms.contains(term) {
                    PATH_WEIGHT
                } else {
                    0.0
                };
                postings
                    .entry(term.clone())
                    .or_default()
                    .push((id, weight + in_path));
            }
            for term in &cached.path_terms {
                if draft.terms.iter().all(|(known, _)| known != term) {
                    postings
                        .entry(term.clone())
                        .or_default()
                        .push((id, PATH_WEIGHT));
                }
            }
            file_length += draft.length;
            if dimension > 0 {
                let start = vectors.len();
                vectors.extend_from_slice(&draft.vector);
                vectors.resize(start + dimension, 0.0);
                for (sum, value) in file_vector.iter_mut().zip(&draft.vector) {
                    *sum += value;
                }
            }
            chunks.push(Chunk {
                file,
                start_line: draft.start_line,
                end_line: draft.end_line,
                names: draft.names.clone(),
                name_lines: draft.name_lines.clone(),
                length: draft.length,
                imports: draft.imports,
            });
        }
        let norm = file_vector
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if norm > 0.0 {
            file_vector.iter_mut().for_each(|value| *value /= norm);
        }
        file_vectors.extend(file_vector);
        for (term, weight) in file_weights {
            file_postings
                .entry(term.to_owned())
                .or_default()
                .push((file, weight));
        }
        total_length += file_length;
        file_lengths.push(file_length);
        cache.insert(relative, cached);
    }

    Index {
        root,
        files,
        average_length: total_length / chunks.len().max(1) as f32,
        chunks,
        postings,
        average_file_length: total_length / file_lengths.len().max(1) as f32,
        file_postings,
        file_lengths,
        vectors,
        file_vectors,
        dimension,
        model,
        tokenizer,
        cache,
        store,
        label: String::new(),
    }
}

fn draft_file(tokenizer: &Tokenizer, model: Option<&StaticModel>, text: &str) -> Vec<Draft> {
    let lines: Vec<&str> = text.lines().collect();
    chunk(&lines)
        .into_iter()
        .map(|span| {
            let body = lines[span.start..span.end].join("\n");
            let (names, name_lines) = names_in(&lines[span.start..span.end], span.start as u32 + 1);
            let mut weights: HashMap<String, f32> = HashMap::new();
            let mut terms = Vec::new();
            tokenizer.terms(&body, &mut terms);
            let length = terms.len() as f32;
            for term in terms.drain(..) {
                *weights.entry(term).or_default() += 1.0;
            }
            tokenizer.terms(&names.join(" "), &mut terms);
            for term in terms.drain(..) {
                *weights.entry(term).or_default() += NAME_WEIGHT;
            }
            // One text per call: a batch pads to its longest member and
            // model2vec averages the padding in, so a vector would depend on
            // its neighbours.
            let vector = model
                .and_then(|model| {
                    model
                        .encode_with_args(std::slice::from_ref(&body), None, 1)
                        .into_iter()
                        .next()
                })
                .unwrap_or_default();
            Draft {
                imports: mostly_imports(&lines[span.start..span.end]),
                start_line: span.start as u32 + 1,
                end_line: span.end as u32,
                names,
                name_lines,
                length,
                terms: weights.into_iter().collect(),
                vector,
            }
        })
        .collect()
}

/// Whether most of what these lines say is what they import.
fn mostly_imports(lines: &[&str]) -> bool {
    const PREFIXES: &[&str] = &[
        "import ", "import(", "use ", "pub use ", "from ", "#include", "require(", "using ",
        "package ", "namespace ", "<?php", "declare(",
    ];
    let mut written = 0;
    let mut importing = 0;
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line == ")" || line == "(" {
            continue;
        }
        written += 1;
        let quoted_path = line.starts_with('"') && line.trim_end_matches(',').ends_with('"');
        let aliased_path =
            line.ends_with('"') && line.split_whitespace().count() == 2 && line.contains('/');
        if quoted_path
            || aliased_path
            || PREFIXES.iter().any(|prefix| line.starts_with(prefix))
            || (line.contains("require(") && line.starts_with("const "))
        {
            importing += 1;
        }
    }
    written > 0 && importing * 5 >= written * 3
}

/// The identifiers a run of lines declares, and the line of each.
fn names_in(lines: &[&str], first_line: u32) -> (Vec<String>, Vec<u32>) {
    let mut names: Vec<String> = Vec::new();
    let mut name_lines = Vec::new();
    for (offset, line) in lines.iter().enumerate() {
        let next = lines[offset + 1..].iter().map(|line| line.trim()).find(|line| !line.is_empty());
        let name = match declared_name(line, next) {
            Some(name) => name.to_owned(),
            // A signature too long for its line: read it as the one line it
            // would have been, and only if what it reaches is a body.
            None => match unwrapped(&lines[offset..]) {
                Some(whole) => match declared_name(&whole, None) {
                    Some(name) => name.to_owned(),
                    None => continue,
                },
                None => continue,
            },
        };
        if names.contains(&name) {
            continue;
        }
        names.push(name);
        name_lines.push(first_line + offset as u32);
        if names.len() == MAX_NAMES {
            break;
        }
    }
    (names, name_lines)
}

/// A top-level line that breaks off inside its parameters, joined with what
/// follows it up to the brace that opens a body. A prototype ends in `;` and
/// is not joined: it declares nothing that is not defined elsewhere.
fn unwrapped(lines: &[&str]) -> Option<String> {
    const MAX_CONTINUATIONS: usize = 8;
    let first = lines.first()?;
    let broken = first.trim_end().ends_with([',', '(']);
    if !broken || first.starts_with(char::is_whitespace) || first.contains(['=', ';']) {
        return None;
    }
    let mut whole = first.trim_end().to_owned();
    for line in lines.iter().skip(1).take(MAX_CONTINUATIONS) {
        let part = line.trim();
        whole.push(' ');
        whole.push_str(part);
        if part.ends_with('{') {
            return Some(whole);
        }
        if part.ends_with(';') || part.is_empty() {
            return None;
        }
    }
    None
}

/// What `line` declares, if it declares anything. `next` is the next line
/// that is not blank, for the brace a declaration may leave to it.
fn declared_name<'a>(line: &'a str, next: Option<&str>) -> Option<&'a str> {
    static DECLARATION: OnceLock<Regex> = OnceLock::new();
    let declaration = DECLARATION.get_or_init(|| {
        Regex::new(
            // A declaring keyword opens its line, behind modifiers at most:
            // prose and markup say `type`, `object` and `record` mid-sentence.
            r#"^(\s*)(?:(?:export|default|declare|pub(?:\([^)]*\))?|async|public|private|protected|static|abstract|final|readonly|override|internal|open|sealed|inline|virtual|partial|data|unsafe|const|typedef|extern(?:\s+"[^"]*")?)\s+)*(fn|struct|enum|trait|impl|mod|type|const|static|union|macro_rules!|class|def|function|interface|func|fun|object|module|record|protocol|extension|defmodule|defp?)(?:<[^>]*>)?\s+(?:\([^)]*\)\s*)?([A-Za-z_$][A-Za-z0-9_$]*)"#,
        )
        .expect("the declaration pattern is a valid regex")
    });
    // A function without a keyword -- C, C++, Java, C#, a TypeScript member --
    // is types and modifiers, a name, parameters, and the brace that opens its
    // body. No `=` and no `;`, so neither a call nor a prototype is one.
    static CALLABLE: OnceLock<Regex> = OnceLock::new();
    let callable = CALLABLE.get_or_init(|| {
        Regex::new(r"^(\s*)((?:[A-Za-z_$][\w$<>\[\],.?:]*[\s*&]+)*)([A-Za-z_$][\w$]*)\s*(?:<[^>()]*>)?\([^;=]*\)[^;=(]*\{\s*$")
            .expect("the callable pattern is a valid regex")
    });
    // `} name_t;` closes a C typedef and is the only line that names it.
    static CLOSER: OnceLock<Regex> = OnceLock::new();
    let closer = CLOSER
        .get_or_init(|| Regex::new(r"^\}\s*([A-Za-z_][A-Za-z0-9_]*)\s*;").expect("the closer pattern is a valid regex"));
    // A constant by its spelling, alone on its line: an enum member, a
    // module-level `MAX_RETRIES = 3`.
    static CONSTANT: OnceLock<Regex> = OnceLock::new();
    let constant = CONSTANT.get_or_init(|| {
        Regex::new(r"^\s*([A-Z_][A-Z0-9_]{2,}[A-Z0-9])\s*(?:=\s*[^=;{(\[]+)?,?\s*(?:(?://|#|/\*).*)?$")
            .expect("the constant pattern is a valid regex")
    });
    static MACRO: OnceLock<Regex> = OnceLock::new();
    let macro_definition = MACRO.get_or_init(|| {
        Regex::new(r"^\s*#\s*define\s+([A-Za-z_][A-Za-z0-9_]*)").expect("the macro pattern is a valid regex")
    });

    let trimmed = line.trim_start();
    if let Some(found) = macro_definition.captures(line) {
        return found.get(1).map(|name| name.as_str());
    }
    if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
        return None;
    }
    if let Some(found) = closer.captures(line).or_else(|| constant.captures(line)) {
        let name = found.get(1)?.as_str();
        return name.chars().any(char::is_alphabetic).then_some(name);
    }

    if let Some(found) = declaration.captures(line) {
        let (indent, keyword) = (&found[1], &found[2]);
        let name = found.get(3)?;
        let rest = line[name.end()..].trim_start();
        // `struct tm now;`, `struct node *next`, `static int count(` use a
        // type; they do not declare one. What follows a declared name is
        // punctuation -- a brace, a colon, generics, nothing -- not another word.
        let uses_a_type = matches!(keyword, "struct" | "enum" | "union" | "static" | "const" | "type")
            && rest.starts_with(|c: char| c == '*' || c == '&' || c == '_' || c.is_alphabetic());
        if !uses_a_type {
            let name = name.as_str();
            // `function use (`, `static function`: a keyword after a declaring
            // keyword is not what is being declared.
            if KEYWORDS.contains(&name) {
                return None;
            }
            // A `const` inside a body is a local, not something a file
            // declares -- unless it is spelled the way constants are, or is a
            // function, which is how a composable or a store declares its own.
            let nested = !indent.is_empty();
            let shouted = !name.chars().any(char::is_lowercase);
            let function = line.contains("=>") || line.contains("function");
            if nested && !shouted && !function && matches!(keyword, "const" | "static") {
                return None;
            }
            return Some(name);
        }
    }

    // The brace may sit on its own line below (`int main(void)` / `{`).
    let found = match callable.captures(line) {
        Some(found) => found,
        None if next == Some("{") && trimmed.trim_end().ends_with(')') => {
            let opened = format!("{} {{", line.trim_end());
            let name = callable.captures(&opened)?.get(3)?.range();
            // Where the name sits in `opened` is where it sits in `line`.
            return checked_callable(&line[name], line.len() - trimmed.len(), true);
        }
        None => return None,
    };
    // `switch len(parts) {`, `return int(count) {`: what precedes the name
    // has to be types and modifiers, not a statement.
    let mut before = found[2].split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|word| !word.is_empty());
    if before.any(|word| CONTROL.contains(&word) || matches!(word, "if" | "for" | "while" | "new")) {
        return None;
    }
    let has_type = !found[2].trim().is_empty();
    let name = found.get(3)?.as_str();
    checked_callable(name, found[1].len(), has_type)
}

fn checked_callable(name: &str, indent: usize, has_type: bool) -> Option<&str> {
    // At the top level a bare `name(args) {` is a call followed by a block in
    // most languages; with a type in front of it, it is a definition.
    if indent == 0 && !has_type {
        return None;
    }
    (!KEYWORDS.contains(&name) && !CONTROL.contains(&name)).then_some(name)
}

/// What is committed to many repositories and is never what anyone is looking
/// for: dependencies, build output, lock files, minified bundles, source maps.
/// An ignore file is still the way to exclude anything else.
fn is_junk(relative: &Path) -> bool {
    let in_junk_dir = relative.parent().is_some_and(|parent| {
        parent.components().any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some(
                    "node_modules" | "vendor" | "vendored" | "third_party" | "third-party" | "dist" | "target"
                        | "__pycache__" | "bower_components"
                )
            )
        })
    });
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    in_junk_dir
        || matches!(
            name,
            "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "composer.lock" | "Cargo.lock"
                | "go.sum" | "poetry.lock" | "Gemfile.lock" | "uv.lock" | "bun.lock"
        )
        || name.ends_with(".map")
        || name.contains(".min.")
        || name.ends_with(".snap")
}

/// A file of checks that does not sit where tests are expected: it is mostly
/// assertions. Rust is left out, because its tests live inside the file they test.
fn reads_as_tests(text: &str) -> bool {
    const MARKS: &[&str] = &[
        "assert.", "assert(", "assertEquals(", "assertTrue(", "assert_eq!(", "expect(", "describe(",
        "it(", "test(", "t.Run(", "t.Fatal", "t.Error", "@Test", "->assert", "self.assert", "PASS:", "FAIL:",
    ];
    let mut lines = 0usize;
    let mut checking = 0usize;
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }
        lines += 1;
        if MARKS.iter().any(|mark| line.starts_with(mark) || line.contains(&format!(" {mark}")) || line.contains(&format!("({mark}"))) {
            checking += 1;
        }
    }
    checking >= 6 && checking * 100 >= lines * 5
}

/// Code written by a bundler rather than a person: a few enormous lines.
fn is_minified(text: &str) -> bool {
    let lines = text.lines().count().max(1);
    text.len() > 4096 && text.len() / lines > 400
}

fn kind_of(relative: &Path) -> Option<Kind> {
    let extension = relative.extension()?.to_str()?.to_ascii_lowercase();
    let kind = match extension.as_str() {
        "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" | "java" | "kt" | "kts"
        | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "cs" | "rb" | "php" | "swift" | "scala"
        | "sh" | "bash" | "ps1" | "sql" | "lua" | "dart" | "ex" | "exs" | "vue" | "svelte" | "astro"
        | "zig" | "hs" | "ml" | "clj" | "r" | "m" | "mm" | "pl" | "proto" | "graphql" | "css"
        | "scss" | "html" => Kind::Code,
        "md" | "mdx" | "rst" | "txt" | "adoc" => Kind::Docs,
        "json" | "toml" | "yaml" | "yml" | "ini" | "xml" | "cfg" | "conf" | "env" => Kind::Config,
        _ => return None,
    };
    if kind == Kind::Code && is_test(relative) {
        return Some(Kind::Test);
    }
    Some(kind)
}

fn is_test(relative: &Path) -> bool {
    let in_test_dir = relative.parent().is_some_and(|parent| {
        parent.components().any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some(
                    "tests" | "test" | "__tests__" | "spec" | "specs" | "fixtures" | "testdata" | "benches"
                        | "e2e" | "cypress" | "playwright" | "__mocks__" | "mocks" | "stories" | "migrations"
                        | "seeders" | "factories"
                )
            )
        })
    });
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    in_test_dir
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
}

#[cfg(test)]
mod tests {
    use super::{declared_name, names_in};

    #[test]
    fn a_wrapped_signature_is_read_as_one_line() {
        let definition = [
            "static TSNode kt_find_extension_receiver(KotlinLSPContext *ctx, TSNode func_node,",
            "                                         char **recv_text_out) {",
            "    return null_node;",
            "}",
        ];
        assert_eq!(names_in(&definition, 314), (vec!["kt_find_extension_receiver".to_owned()], vec![314]));
        // The same, ending in `;`, is a prototype and declares nothing here.
        let prototype = ["int cbm_store_upsert(cbm_store_t *store, const cbm_node_t *node,", "                     int flags);"];
        assert_eq!(names_in(&prototype, 1).0, Vec::<String>::new());
    }

    #[test]
    fn what_a_line_declares() {
        let cases: &[(&str, Option<&str>, Option<&str>)] = &[
            // C: the name, not the return type; every one of them, not the first `void`.
            ("static int bind_text(sqlite3_stmt *s, int col, const char *v) {", None, Some("bind_text")),
            ("static const char *safe_str(const char *s) {", None, Some("safe_str")),
            ("cbm_store_t *cbm_store_open(const char *path)", Some("{"), Some("cbm_store_open")),
            ("resolve_import(ctx_t *ctx, const char *path)", Some("{"), Some("resolve_import")),
            ("struct cbm_store {", None, Some("cbm_store")),
            ("typedef struct cbm_node {", None, Some("cbm_node")),
            ("    struct tm now;", None, None),
            ("struct node *make_node(void) {", None, Some("make_node")),
            ("#define CBM_MAX_DEPTH 64", None, Some("CBM_MAX_DEPTH")),
            ("int cbm_store_close(cbm_store_t *s);", None, None),
            ("} cbm_louvain_edge_t;", None, Some("cbm_louvain_edge_t")),
            ("    }", None, None),
            ("    CBM_SVC_ROUTE_REG = 4,", None, Some("CBM_SVC_ROUTE_REG")),
            ("    CBM_SVC_NONE,  /* nothing */", None, Some("CBM_SVC_NONE")),
            ("CACHE_FORMAT_VERSION = 1  # Bump when the schema changes.", None, Some("CACHE_FORMAT_VERSION")),
            ("    OK", None, None),
            ("    RESULT = compute(a, b)", None, None),
            ("    if (X == Y_Z) {", None, None),
            ("    if (rc != SQLITE_OK) {", None, None),
            ("\tswitch len(parts) {", None, None),
            ("\t\treturn int(count) {", None, None),
            ("    } else if (ready(x)) {", None, None),
            ("    return helper(a, b);", None, None),
            // Rust
            ("pub(crate) async fn refreshed(self) -> Self {", None, Some("refreshed")),
            ("static DECLARATION: OnceLock<Regex> = OnceLock::new();", None, Some("DECLARATION")),
            ("impl<T> Shelf<T> {", None, Some("Shelf")),
            // TypeScript / Vue
            ("  async onResponse({ response }): Promise<void> {", None, Some("onResponse")),
            ("export const useCart = (key: string) => {", None, Some("useCart")),
            ("  const total = items.length;", None, None),
            ("const v$ = useValidate(rules, form)", None, Some("v$")),
            ("describe('cart', () => {", None, None),
            // Go, Python, PHP
            ("func (s *Session) ValidateToken(token string) error {", None, Some("ValidateToken")),
            ("    def chunk_file(self, path: str) -> list[Chunk]:", None, Some("chunk_file")),
            ("    public function toPayload(): OrderPayload", Some("{"), Some("toPayload")),
            ("        $callback = function () use ($user) {", None, None),
        ];
        for (line, next, expected) in cases {
            assert_eq!(declared_name(line, *next), *expected, "{line}");
        }
    }
}
