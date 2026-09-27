use crate::chunk::chunk;
use crate::roots;
use crate::store::{self, FileRecord, Segment, Stamp, VectorRows, Writer, quantize};
use crate::tokenize::Tokenizer;
use model2vec_rs::model::StaticModel;
use rayon::prelude::*;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// What a file is for, which decides whether a search looks at it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// The files and chunks of the tree as it is, read from segments: those of
/// the store, and a delta of the files read again since.
///
/// The tables every answer walks -- files, chunks, their names -- are held
/// here; the postings and vectors stay in the segments, read where they lie.
pub struct Index {
    pub root: PathBuf,
    pub files: Vec<FileEntry>,
    pub chunks: Vec<Chunk>,
    pub file_lengths: Vec<f32>,
    pub average_length: f32,
    pub average_file_length: f32,
    pub model: Option<Arc<StaticModel>>,
    pub tokenizer: Tokenizer,
    /// What an answer puts in front of each path: nothing for the directory
    /// the agent is taken to be in, the root itself for anywhere else, so that
    /// a path in an answer can be opened as it stands.
    pub label: String,
    segments: Vec<Arc<Segment>>,
    delta: Option<Arc<Segment>>,
    /// For each segment, then the delta: where its chunks and files are in
    /// the tables above, `DEAD` for those the tree no longer has as they were.
    maps: Vec<Map>,
    /// For each file of the tables, which part holds it and where.
    sources: Vec<(u32, u32)>,
    /// The headings of documents by their words: built on the first search
    /// after the tables change, not on every one.
    headings: OnceLock<HashMap<String, Vec<(u32, u32)>>>,
    /// The files read since the segments were written, which the delta holds.
    dirty: BTreeMap<String, CachedFile>,
    /// The tree as last walked.
    walked: Walked,
    /// Where the segments are kept between runs, when they are.
    store: Option<PathBuf>,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("root", &self.root)
            .field("files", &self.files.len())
            .field("chunks", &self.chunks.len())
            .field("segments", &self.segments.len())
            .finish_non_exhaustive()
    }
}

const DEAD: u32 = u32::MAX;

#[derive(Debug)]
struct Map {
    chunks: Vec<u32>,
    files: Vec<u32>,
    /// Chunks still live.
    live: usize,
}

/// A name is worth this many body occurrences, and a path word this many.
/// Both are whole: a posting's weight is a sum of them, stored as an integer.
const NAME_WEIGHT: f32 = 3.0;
const PATH_WEIGHT: f32 = 2.0;
const _: () = assert!(NAME_WEIGHT as u32 as f32 == NAME_WEIGHT && PATH_WEIGHT as u32 as f32 == PATH_WEIGHT);
const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_NAMES: usize = 96;
/// A first index writes what it has read every this many files, so that one
/// interrupted -- an agent closed, a machine asleep -- resumes where it stopped.
const BATCH_FILES: usize = 2048;
/// Files read again during a session are written out once there are this many.
const PERSIST_DIRTY: usize = 256;
/// Past this many segments, or this share of dead chunks, the store is merged
/// into one segment: each segment costs a lookup per term, and dead chunks
/// cost disk.
const MAX_SEGMENTS: usize = 8;
const DEAD_SHARE: f32 = 0.25;
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

/// A file as it was read, before it is written into a segment.
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
    hash: [u8; 16],
}

struct Draft {
    imports: bool,
    start_line: u32,
    end_line: u32,
    names: Vec<String>,
    name_lines: Vec<u32>,
    length: f32,
    terms: Vec<(String, f32)>,
    /// Empty when the file was read without the model.
    vector: Vec<f32>,
}

type Walked = Vec<(String, PathBuf, Kind, Stamp)>;

/// How far an index has come, for whoever is waiting on it.
#[derive(Clone, Copy, Debug)]
pub enum Progress {
    /// Files read for their words, of those that had to be.
    Reading { done: usize, total: usize },
    /// Files embedded, of those that had to be.
    Embedding { done: usize, total: usize },
    /// The store merged into one segment.
    Compacting,
}

impl Index {
    /// Read every indexable file under `root` and build the channels, keeping
    /// nothing on disk.
    pub fn build(root: &Path, model_dir: Option<&Path>) -> Result<Self, String> {
        Self::open_in(root, model_dir, None)
    }

    /// The same, keeping what each file was read as on disk between runs, so
    /// a later start reads only the files that changed since.
    pub fn open(root: &Path, model_dir: Option<&Path>) -> Result<Self, String> {
        Self::open_in(root, model_dir, cache_dir().as_deref())
    }

    /// `open`, with the store kept under `cache_dir` instead of the user's.
    pub fn open_in(root: &Path, model_dir: Option<&Path>, cache_dir: Option<&Path>) -> Result<Self, String> {
        let (index, upkeep) = Self::open_lexical_in(root, model_dir, cache_dir, &|_| {})?;
        upkeep.run(&|_| {})?;
        Ok(index)
    }

    /// An index every file of which has been read for its words, and what is
    /// left to do: the vectors of files read without the model, and merging
    /// the store. Words are a fifth of the work of a first index, and all that
    /// `usages`, `grep` and `outline` need; the rest can follow.
    pub fn open_lexical(
        root: &Path,
        model_dir: Option<&Path>,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<(Self, Upkeep), String> {
        Self::open_lexical_in(root, model_dir, cache_dir().as_deref(), progress)
    }

    pub fn open_lexical_in(
        root: &Path,
        model_dir: Option<&Path>,
        cache_dir: Option<&Path>,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<(Self, Upkeep), String> {
        let store = cache_dir.and_then(|dir| store_path(dir, root, model_dir));
        // The model, the store and the tree do not depend on one another, and
        // each is a good part of a start: they are read side by side.
        let (model, opened, walked) = std::thread::scope(|scope| {
            let model = scope.spawn(|| {
                model_dir
                    .map(|dir| {
                        StaticModel::from_pretrained(dir, None, Some(true), None)
                            .map_err(|error| format!("cannot load model from {}: {error}", dir.display()))
                    })
                    .transpose()
            });
            let opened = scope.spawn(|| store.as_deref().map(store::open).unwrap_or_default());
            let walked = walk(root);
            (model.join(), opened.join(), walked)
        });
        let model = model.map_err(|_| "loading the model panicked")??.map(Arc::new);
        let mut segments = opened.map_err(|_| "reading the index panicked")?.segments;
        if let Some(store) = &store {
            forget_legacy(store);
            store::touch(store);
        }

        let tokenizer = Tokenizer::new();
        if let (Some(store), Some(cache_dir)) = (&store, cache_dir) {
            let parts: Vec<&Arc<Segment>> = segments.iter().collect();
            let missing: Vec<usize> = resolve(&parts, &walked)
                .iter()
                .enumerate()
                .filter_map(|(at, found)| found.is_none().then_some(at))
                .collect();
            let records = borrowed(root, model_dir, cache_dir, &walked, &missing);
            let mut kept = Vec::new();
            for record in &records {
                if store::append(store, record).is_err() {
                    kept.extend(Segment::owned(record.clone()));
                }
            }
            if !records.is_empty() {
                segments = store::open(store).segments;
                segments.extend(kept);
            }
        }
        // A batch written to the store is read back from it where it lies, not
        // kept in memory: a first index of a large repository would otherwise
        // hold all of itself twice. Should the store lose a batch -- another
        // process merged it meanwhile -- what is missing is read again, and the
        // last attempt keeps what it reads in memory whatever the store does.
        for attempt in 0..3 {
            let parts: Vec<&Arc<Segment>> = segments.iter().collect();
            let missing: Vec<usize> = resolve(&parts, &walked)
                .iter()
                .enumerate()
                .filter_map(|(at, found)| found.is_none().then_some(at))
                .collect();
            if missing.is_empty() {
                break;
            }
            let keep = store.is_none() || attempt == 2;
            let total = missing.len();
            progress(Progress::Reading { done: 0, total });
            let mut kept = Vec::new();
            for (batch, files) in missing.chunks(BATCH_FILES).enumerate() {
                let read: Vec<(&str, CachedFile)> = files
                    .par_iter()
                    .map(|&at| {
                        let (relative, path, kind, stamp) = &walked[at];
                        (relative.as_str(), read_file(&tokenizer, None, relative, path, *kind, *stamp))
                    })
                    .collect();
                let record = write_files(read.iter().map(|(relative, cached)| (*relative, cached)));
                // A store that cannot be written is only a slower next start.
                let stored = !keep && store.as_deref().is_some_and(|store| store::append(store, &record).is_ok());
                if !stored {
                    kept.push(Segment::owned(record).ok_or("a segment just built cannot be read back")?);
                }
                progress(Progress::Reading { done: (batch * BATCH_FILES + files.len()).min(total), total });
            }
            if let (Some(store), false) = (&store, keep) {
                segments = store::open(store).segments;
            }
            segments.extend(kept);
        }

        let mut index = Self {
            root: root.to_path_buf(),
            files: Vec::new(),
            chunks: Vec::new(),
            file_lengths: Vec::new(),
            average_length: 0.0,
            average_file_length: 0.0,
            model,
            tokenizer,
            label: String::new(),
            segments,
            delta: None,
            maps: Vec::new(),
            sources: Vec::new(),
            headings: OnceLock::new(),
            dirty: BTreeMap::new(),
            walked,
            store,
        };
        index.materialize();
        let upkeep = index.upkeep();
        Ok((index, upkeep))
    }

    /// The index of the tree as it is now. Files whose size and modification
    /// time are what they were are not read again; when nothing changed at
    /// all the index is handed back untouched.
    ///
    /// What changed is found by comparing this walk with the last, and the
    /// tables are merged rather than rebuilt: what did not change is moved
    /// into place, not read out of the segments again. An edit then costs
    /// what walking the tree costs, and little more, whatever its size.
    #[must_use]
    pub fn refreshed(self) -> Self {
        self.refreshed_full().0
    }

    #[must_use]
    pub fn refreshed_full(self) -> (Self, bool) {
        let walked = walk(&self.root);
        let changed = walked != self.walked;
        (self.refreshed_to(walked), changed)
    }

    #[must_use]
    pub fn refreshed_with(self, paths: &[PathBuf]) -> Self {
        if paths.is_empty() {
            return self;
        }
        let found = walk_within(&self.root, paths);
        let within = |path: &Path| paths.iter().any(|changed| path.starts_with(changed));
        let mut walked: Walked = self.walked.iter().filter(|(_, path, ..)| !within(path)).cloned().collect();
        walked.extend(found);
        walked.sort_by(|a, b| a.0.cmp(&b.0));
        walked.dedup_by(|a, b| a.0 == b.0);
        self.refreshed_to(walked)
    }

    fn refreshed_to(mut self, walked: Walked) -> Self {
        if walked == self.walked {
            return self;
        }
        let (changed, gone) = differences(&self.walked, &walked);
        self.dirty.retain(|relative, _| !gone.contains(relative.as_str()));
        let model = self.model.as_deref();
        let tokenizer = &self.tokenizer;
        let read: Vec<(String, CachedFile)> = changed
            .par_iter()
            .map(|&at| {
                let (relative, path, kind, stamp) = &walked[at];
                (relative.clone(), read_file(tokenizer, model, relative, path, *kind, *stamp))
            })
            .collect();
        self.dirty.extend(read);
        self.walked = walked;
        let old_delta = self.delta.take().map(|_| self.segments.len() as u32);
        if !self.dirty.is_empty() {
            self.delta =
                Segment::owned(write_files(self.dirty.iter().map(|(relative, cached)| (relative.as_str(), cached))));
        }
        self.merge_tables(old_delta, &gone);
        if self.dirty.len() >= PERSIST_DIRTY {
            self.persist_delta();
        }
        self
    }

    /// Writes the files read again during the session into the store, so the
    /// next start does not read them again.
    pub fn persist(&mut self) {
        self.persist_delta();
    }

    fn persist_delta(&mut self) {
        let (Some(store), Some(delta)) = (&self.store, &self.delta) else {
            return;
        };
        let Some(record) = delta.owned_record() else { return };
        if store::append(store, record).is_ok() {
            // Its map is the last already, as the last segment it now is.
            self.segments.extend(self.delta.take());
            self.dirty.clear();
        }
    }

    /// The tables, from the newest entry of each file in the tree.
    fn materialize(&mut self) {
        let parts: Vec<&Arc<Segment>> = self.segments.iter().chain(self.delta.iter()).collect();
        let mut tables = Tables::default();
        for &(part, local) in resolve(&parts, &self.walked).iter().flatten() {
            if parts[part].file(local).indexed {
                tables.push_from(parts[part], part as u32, local);
            }
        }
        self.install(tables);
    }

    /// The tables with the entries of `gone` files and of the old delta taken
    /// out, and the files of the new delta put in, in path order.
    fn merge_tables(&mut self, old_delta: Option<u32>, gone: &HashSet<String>) {
        let delta_part = self.segments.len() as u32;
        let delta = self.delta.clone();
        let mut fresh = delta
            .as_deref()
            .map(|delta| (0..delta.file_count()).filter(|&local| delta.file(local).indexed).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .peekable();
        let mut tables = Tables::default();
        let mut old_chunks = std::mem::take(&mut self.chunks).into_iter().peekable();
        let old_files = std::mem::take(&mut self.files);
        let old_lengths = std::mem::take(&mut self.file_lengths);
        let old_sources = std::mem::take(&mut self.sources);
        for (old, ((entry, length), source)) in old_files.into_iter().zip(old_lengths).zip(old_sources).enumerate() {
            if let Some(delta) = delta.as_deref() {
                while let Some(&local) = fresh.peek().filter(|&&local| delta.file(local).path < entry.path.as_str()) {
                    tables.push_from(delta, delta_part, local);
                    fresh.next();
                }
            }
            let keep = Some(source.0) != old_delta && !gone.contains(&entry.path);
            let file = tables.files.len() as u32;
            while let Some(chunk) = old_chunks.next_if(|chunk| chunk.file == old as u32) {
                if keep {
                    tables.chunks.push(Chunk { file, ..chunk });
                }
            }
            if keep {
                tables.files.push(entry);
                tables.lengths.push(length);
                tables.sources.push(source);
            }
        }
        if let Some(delta) = delta.as_deref() {
            for local in fresh {
                tables.push_from(delta, delta_part, local);
            }
        }
        self.install(tables);
    }

    /// Takes `tables` as the index's, and points every segment's map at them.
    fn install(&mut self, tables: Tables) {
        let parts: Vec<&Arc<Segment>> = self.segments.iter().chain(self.delta.iter()).collect();
        let mut maps: Vec<Map> = parts
            .iter()
            .map(|segment| Map {
                chunks: vec![DEAD; segment.chunk_count()],
                files: vec![DEAD; segment.file_count()],
                live: 0,
            })
            .collect();
        let mut chunk = 0u32;
        for (file, &(part, local)) in tables.sources.iter().enumerate() {
            let record = parts[part as usize].file(local as usize);
            let map = &mut maps[part as usize];
            map.files[local as usize] = file as u32;
            for local_chunk in record.chunk_start..record.chunk_start + record.chunk_count {
                map.chunks[local_chunk as usize] = chunk;
                chunk += 1;
            }
            map.live += record.chunk_count as usize;
        }
        let total_length: f32 = tables.lengths.iter().sum();
        self.average_length = total_length / tables.chunks.len().max(1) as f32;
        self.average_file_length = total_length / tables.lengths.len().max(1) as f32;
        self.files = tables.files;
        self.chunks = tables.chunks;
        self.file_lengths = tables.lengths;
        self.sources = tables.sources;
        self.maps = maps;
        self.headings = OnceLock::new();
    }

    /// Every word of every document heading, lower-cased as a query's words
    /// are, with the chunk and the place among its names of each heading.
    #[must_use]
    pub fn headings(&self) -> &HashMap<String, Vec<(u32, u32)>> {
        self.headings.get_or_init(|| {
            let mut headings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
            for (id, chunk) in self.chunks.iter().enumerate() {
                if self.files[chunk.file as usize].kind != Kind::Docs {
                    continue;
                }
                for (at, name) in chunk.names.iter().enumerate() {
                    for word in crate::search::plain_words(name) {
                        headings.entry(word).or_default().push((id as u32, at as u32));
                    }
                }
            }
            headings
        })
    }

    fn parts(&self) -> impl Iterator<Item = (&Arc<Segment>, &Map)> {
        self.segments.iter().chain(self.delta.iter()).zip(&self.maps)
    }

    /// Every live chunk `term` occurs in, with its weight there.
    #[must_use]
    pub fn postings(&self, term: &str) -> Vec<(u32, f32)> {
        let mut found = Vec::new();
        for (segment, map) in self.parts().filter(|(_, map)| map.live > 0) {
            segment.postings(term, |local, weight| {
                if let Some(&chunk) = map.chunks.get(local as usize).filter(|&&chunk| chunk != DEAD) {
                    found.push((chunk, weight as f32));
                }
            });
        }
        found
    }

    /// Every live file `term` occurs in, its path included, with its weight there.
    #[must_use]
    pub fn file_postings(&self, term: &str) -> Vec<(u32, f32)> {
        let mut found = Vec::new();
        for (segment, map) in self.parts() {
            segment.file_postings(term, |local, weight| {
                if let Some(&file) = map.files.get(local as usize).filter(|&&file| file != DEAD) {
                    found.push((file, weight as f32));
                }
            });
        }
        found
    }

    /// The dimension of the vectors, once every live chunk has one.
    #[must_use]
    pub fn dense(&self) -> Option<usize> {
        self.model.as_ref()?;
        let mut dimension = None;
        for (segment, _) in self.parts().filter(|(_, map)| map.live > 0) {
            dimension = Some(segment.vectors()?.dimension);
        }
        dimension
    }

    /// Whether vectors are still being computed for some of the chunks.
    #[must_use]
    pub fn embedding(&self) -> bool {
        self.model.is_some() && !self.chunks.is_empty() && self.dense().is_none()
    }

    /// Each chunk's closeness to `asked`.
    #[must_use]
    pub fn chunk_scores(&self, asked: &[f32]) -> Vec<f32> {
        let mut scores = vec![0.0f32; self.chunks.len()];
        for (segment, map) in self.parts().filter(|(_, map)| map.live > 0) {
            let Some(vectors) = segment.vectors() else { continue };
            let local: Vec<f32> = (0..map.chunks.len())
                .into_par_iter()
                .map(|chunk| if map.chunks[chunk] == DEAD { 0.0 } else { vectors.chunk_dot(chunk, asked) })
                .collect();
            for (chunk, score) in map.chunks.iter().zip(local) {
                if *chunk != DEAD {
                    scores[*chunk as usize] = score;
                }
            }
        }
        scores
    }

    /// Each file's closeness to `asked`, by the direction its chunks share.
    #[must_use]
    pub fn file_scores(&self, asked: &[f32]) -> Vec<f32> {
        let mut scores = vec![0.0f32; self.files.len()];
        for (segment, map) in self.parts() {
            let Some(vectors) = segment.vectors() else { continue };
            for (local, &file) in map.files.iter().enumerate() {
                if file != DEAD {
                    scores[file as usize] = vectors.file_dot(local, asked);
                }
            }
        }
        scores
    }

    /// What is left to do after the words: the vectors of the segments read
    /// without the model, and merging the store once it has grown ragged.
    #[must_use]
    pub fn upkeep(&self) -> Upkeep {
        let pending = if self.model.is_some() {
            self.parts()
                .filter(|(segment, map)| map.live > 0 && segment.vectors().is_none())
                .map(|(segment, _)| Arc::clone(segment))
                .collect()
        } else {
            Vec::new()
        };
        let total: usize = self.segments.iter().map(|segment| segment.chunk_count()).sum();
        let live: usize = self.maps.iter().take(self.segments.len()).map(|map| map.live).sum();
        let ragged = self.segments.len() > MAX_SEGMENTS || (total - live) as f32 > total as f32 * DEAD_SHARE;
        Upkeep {
            root: self.root.clone(),
            model: self.model.clone(),
            store: self.store.clone(),
            pending,
            compact: ragged,
        }
    }

    /// Where the store is, when there is one.
    #[must_use]
    pub fn store(&self) -> Option<&Path> {
        self.store.as_deref()
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

/// The work an index leaves for later: it holds what it needs, so it can be
/// done on another thread while the index answers.
#[derive(Debug)]
pub struct Upkeep {
    root: PathBuf,
    model: Option<Arc<StaticModel>>,
    store: Option<PathBuf>,
    pending: Vec<Arc<Segment>>,
    compact: bool,
}

impl Upkeep {
    /// Whether there is nothing to do.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.pending.is_empty() && !(self.compact && self.store.is_some())
    }

    /// Merge the store into one segment whatever its state: what `omega
    /// index` leaves behind is the smallest store there is.
    #[must_use]
    pub fn tidy(mut self) -> Self {
        self.compact = true;
        self
    }

    /// Embeds what is pending, segment by segment, each written to the store
    /// as soon as it is done and handed to the index that shares it; then
    /// merges the store when asked to.
    pub fn run(self, progress: &(dyn Fn(Progress) + Sync)) -> Result<(), String> {
        if let Some(model) = &self.model {
            let total: usize = self.pending.iter().map(|segment| segment.file_count()).sum();
            let mut done = 0;
            if total > 0 {
                progress(Progress::Embedding { done, total });
            }
            let dimension = model
                .encode_with_args(&["dimension".to_owned()], None, 1)
                .first()
                .map_or(0, Vec::len);
            for segment in &self.pending {
                let rows = embed(&self.root, model, dimension, segment);
                if let Some(store) = &self.store {
                    let _ = store::append(store, &rows.attachment(segment.id));
                }
                if let Some(vectors) = rows.into_vectors() {
                    segment.attach(vectors);
                }
                done += segment.file_count();
                progress(Progress::Embedding { done, total });
            }
        }
        if let (Some(store), true) = (&self.store, self.compact) {
            progress(Progress::Compacting);
            compact(&self.root, store)?;
        }
        Ok(())
    }
}

/// The vectors of a segment's chunks, from the files as they are on disk. A
/// file changed since it was read is read again anyway, into a newer segment;
/// its rows here are never looked at, and left empty.
///
/// Files are read side by side, then chunks embedded side by side: one large
/// file among many small ones would otherwise leave every core but one idle.
fn embed(root: &Path, model: &StaticModel, dimension: usize, segment: &Segment) -> VectorRows {
    // Each file's text and where each of its lines lies in it, as `lines()` cuts them.
    type Text = (String, Vec<(usize, usize)>);
    let texts: Vec<Option<Text>> = (0..segment.file_count())
        .into_par_iter()
        .map(|local| {
            let record = segment.file(local);
            if !record.indexed || record.chunk_count == 0 {
                return None;
            }
            let path = root.join(record.path);
            if stamp_of(&std::fs::metadata(&path).ok()?) != record.stamp {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            let base = text.as_ptr() as usize;
            let lines = text
                .lines()
                .map(|line| {
                    let start = line.as_ptr() as usize - base;
                    (start, start + line.len())
                })
                .collect();
            Some((text, lines))
        })
        .collect();
    let vectors: Vec<Vec<f32>> = (0..segment.chunk_count())
        .into_par_iter()
        .map(|chunk| {
            let chunk = segment.chunk(chunk);
            let Some((text, lines)) = &texts[chunk.file as usize] else {
                return Vec::new();
            };
            let start = (chunk.start_line as usize).saturating_sub(1).min(lines.len());
            let end = (chunk.end_line as usize).clamp(start, lines.len());
            // One text per call, as when a file is read with the model.
            let body = lines[start..end].iter().map(|&(from, to)| &text[from..to]).collect::<Vec<_>>().join("\n");
            model
                .encode_with_args(std::slice::from_ref(&body), None, 1)
                .into_iter()
                .next()
                .unwrap_or_default()
        })
        .collect();
    let mut rows = VectorRows::new(dimension);
    for (local, text) in texts.iter().enumerate() {
        let record = segment.file(local);
        let range = record.chunk_start as usize..(record.chunk_start + record.chunk_count) as usize;
        if text.is_some() {
            let (row, scale) = quantize(&file_vector(vectors[range.clone()].iter().map(Vec::as_slice), dimension));
            rows.file(&row, scale);
        } else {
            rows.file(&[], 0.0);
        }
        for vector in &vectors[range] {
            let (row, scale) = quantize(vector);
            rows.chunk(&row, scale);
        }
    }
    rows
}

/// The direction a file's chunks share: their sum, made unit.
fn file_vector<'a>(vectors: impl Iterator<Item = &'a [f32]>, dimension: usize) -> Vec<f32> {
    let mut sum = vec![0.0f32; dimension];
    for vector in vectors {
        for (sum, value) in sum.iter_mut().zip(vector) {
            *sum += value;
        }
    }
    let norm = sum.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        sum.iter_mut().for_each(|value| *value /= norm);
    }
    sum
}

/// The tables of an index being put together, in path order.
#[derive(Default)]
struct Tables {
    files: Vec<FileEntry>,
    chunks: Vec<Chunk>,
    lengths: Vec<f32>,
    /// For each file, which part holds it and where.
    sources: Vec<(u32, u32)>,
}

impl Tables {
    /// The file at `local` of `segment`, the part numbered `part`, and its chunks.
    fn push_from(&mut self, segment: &Segment, part: u32, local: usize) {
        let record = segment.file(local);
        let file = self.files.len() as u32;
        self.files.push(FileEntry {
            path: record.path.to_owned(),
            kind: record.kind,
        });
        self.lengths.push(record.length);
        self.sources.push((part, local as u32));
        for local_chunk in record.chunk_start..record.chunk_start + record.chunk_count {
            let chunk = segment.chunk(local_chunk as usize);
            let (names, name_lines) = segment.names(&chunk).map(|(name, line)| (name.to_owned(), line)).unzip();
            self.chunks.push(Chunk {
                file,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                names,
                name_lines,
                length: chunk.length,
                imports: chunk.imports,
            });
        }
    }
}

/// Between two walks, both in path order: which files of `after` are new or
/// changed, and which paths of `before` no longer describe the tree -- gone,
/// or changed.
fn differences(before: &Walked, after: &Walked) -> (Vec<usize>, HashSet<String>) {
    let mut changed = Vec::new();
    let mut gone = HashSet::new();
    let (mut old, mut new) = (0, 0);
    loop {
        match (before.get(old), after.get(new)) {
            (Some(was), Some(is)) => match was.0.cmp(&is.0) {
                std::cmp::Ordering::Less => {
                    gone.insert(was.0.clone());
                    old += 1;
                }
                std::cmp::Ordering::Greater => {
                    changed.push(new);
                    new += 1;
                }
                std::cmp::Ordering::Equal => {
                    if was.2 != is.2 || was.3 != is.3 {
                        gone.insert(was.0.clone());
                        changed.push(new);
                    }
                    old += 1;
                    new += 1;
                }
            },
            (Some(was), None) => {
                gone.insert(was.0.clone());
                old += 1;
            }
            (None, Some(_)) => {
                changed.push(new);
                new += 1;
            }
            (None, None) => return (changed, gone),
        }
    }
}

/// For each file of the tree, the newest entry that describes it as it is:
/// which of `parts` holds it, and where.
fn resolve(parts: &[&Arc<Segment>], walked: &Walked) -> Vec<Option<(usize, usize)>> {
    let wanted: HashMap<&str, usize> =
        walked.iter().enumerate().map(|(at, (relative, ..))| (relative.as_str(), at)).collect();
    let mut found = vec![None; walked.len()];
    for (part, segment) in parts.iter().enumerate().rev() {
        for local in 0..segment.file_count() {
            let record = segment.file(local);
            let Some(&at) = wanted.get(record.path) else { continue };
            let (_, _, kind, stamp) = &walked[at];
            if found[at].is_none() && record.stamp == *stamp && record.walked == *kind {
                found[at] = Some((part, local));
            }
        }
    }
    found
}

/// Rewrites the store as one segment of what is live in it, when it has grown
/// ragged: many segments, or many dead chunks. Read afresh from disk, so that
/// what other processes appended is kept too.
fn compact(root: &Path, path: &Path) -> Result<(), String> {
    let opened = store::open(path);
    let walked = walk(root);
    let parts: Vec<&Arc<Segment>> = opened.segments.iter().collect();
    let live: Vec<(usize, usize)> = resolve(&parts, &walked).into_iter().flatten().collect();
    let total: usize = parts.iter().map(|segment| segment.chunk_count()).sum();
    let live_chunks: usize = live.iter().map(|&(part, local)| parts[part].file(local).chunk_count as usize).sum();
    let listed: usize = parts.iter().map(|segment| segment.file_count()).sum();
    if parts.len() <= 1 && live_chunks == total && live.len() == listed && opened.clean() {
        return Ok(());
    }
    // A merged segment has vectors for all its chunks or none. While some live
    // chunks still wait for theirs -- another process is reading files into
    // the store as this one merges -- merging would throw away every vector
    // there is: it waits for the next time instead.
    let with_chunks = || live.iter().filter(|&&(part, local)| parts[part].file(local).chunk_count > 0);
    let embedded = with_chunks().filter(|&&(part, _)| parts[part].vectors().is_some()).count();
    if embedded > 0 && embedded < with_chunks().count() {
        return Ok(());
    }
    let dimension = with_chunks()
        .find_map(|&(part, _)| parts[part].vectors().map(|vectors| vectors.dimension))
        .unwrap_or(0);
    let record = merge(&parts, &live, None, dimension);
    store::replace(path, &[&record]).map_err(|error| format!("cannot rewrite {}: {error}", path.display()))
}

/// One segment holding the `live` files of `parts`, in that order.
fn merge(parts: &[&Arc<Segment>], live: &[(usize, usize)], stamps: Option<&[Stamp]>, dimension: usize) -> Vec<u8> {
    let mut writer = Writer::new(dimension);
    let mut chunk_maps: Vec<Vec<u32>> = parts.iter().map(|segment| vec![DEAD; segment.chunk_count()]).collect();
    let mut file_maps: Vec<Vec<u32>> = parts.iter().map(|segment| vec![DEAD; segment.file_count()]).collect();
    let mut next_chunk = 0u32;
    for (file, &(part, local)) in live.iter().enumerate() {
        let segment = parts[part];
        let mut record = segment.file(local);
        if let Some(stamps) = stamps {
            record.stamp = stamps[file];
        }
        file_maps[part][local] = file as u32;
        let vectors = segment.vectors().filter(|_| dimension > 0);
        writer.file(&record, vectors.map(|vectors| vectors.file_row(local)));
        for local_chunk in record.chunk_start..record.chunk_start + record.chunk_count {
            let chunk = segment.chunk(local_chunk as usize);
            chunk_maps[part][local_chunk as usize] = next_chunk;
            next_chunk += 1;
            writer.chunk(
                chunk.start_line,
                chunk.end_line,
                chunk.length,
                chunk.imports,
                segment.names(&chunk),
                vectors.map(|vectors| vectors.chunk_row(local_chunk as usize)),
            );
        }
    }
    // Every term of every part, in byte order, with the postings still live.
    let mut heap: BinaryHeap<std::cmp::Reverse<(&[u8], usize, usize)>> = parts
        .iter()
        .enumerate()
        .filter(|(_, segment)| segment.term_count() > 0)
        .map(|(part, segment)| std::cmp::Reverse((segment.term(0), part, 0)))
        .collect();
    let mut postings = Vec::new();
    let mut file_postings = Vec::new();
    while let Some(std::cmp::Reverse((term, ..))) = heap.peek().copied() {
        postings.clear();
        file_postings.clear();
        while let Some(&std::cmp::Reverse((next, part, at))) = heap.peek() {
            if next != term {
                break;
            }
            heap.pop();
            let segment = parts[part];
            segment.postings_at(at, |local, weight| {
                let chunk = chunk_maps[part][local as usize];
                if chunk != DEAD {
                    postings.push((chunk, weight));
                }
            });
            segment.file_postings_at(at, |local, weight| {
                let file = file_maps[part][local as usize];
                if file != DEAD {
                    file_postings.push((file, weight));
                }
            });
            if at + 1 < segment.term_count() {
                heap.push(std::cmp::Reverse((segment.term(at + 1), part, at + 1)));
            }
        }
        if postings.is_empty() && file_postings.is_empty() {
            continue;
        }
        postings.sort_unstable();
        file_postings.sort_unstable();
        writer.term(term, &postings, &file_postings);
    }
    writer.finish(store::new_id())
}

/// A segment of `files`, in the order given, which is path order.
fn write_files<'a>(files: impl Iterator<Item = (&'a str, &'a CachedFile)>) -> Vec<u8> {
    let files: Vec<(&str, &CachedFile)> = files.collect();
    let dimension = files
        .iter()
        .flat_map(|(_, cached)| cached.drafts.iter().map(|draft| draft.vector.len()))
        .max()
        .unwrap_or(0);
    let mut writer = Writer::new(dimension);
    let mut postings: HashMap<&str, Vec<(u32, u32)>> = HashMap::new();
    let mut file_postings: HashMap<&str, Vec<(u32, u32)>> = HashMap::new();
    let whole = |weight: f32| weight.round().max(0.0) as u32;
    let mut next_chunk = 0u32;
    for (file, (relative, cached)) in files.iter().enumerate() {
        let file = file as u32;
        let vector = (dimension > 0).then(|| {
            quantize(&file_vector(cached.drafts.iter().map(|draft| draft.vector.as_slice()), dimension))
        });
        writer.file(
            &FileRecord {
                path: relative,
                stamp: cached.stamp,
                walked: cached.walked,
                kind: cached.kind,
                indexed: cached.indexed,
                chunk_start: 0,
                chunk_count: 0,
                length: cached.drafts.iter().map(|draft| draft.length).sum(),
                hash: cached.hash,
            },
            vector.as_ref().map(|(row, scale)| (row.as_slice(), *scale)),
        );
        if !cached.indexed {
            continue;
        }
        let mut file_weights: HashMap<&str, f32> = HashMap::new();
        for term in &cached.path_terms {
            *file_weights.entry(term).or_default() += PATH_WEIGHT;
        }
        for draft in &cached.drafts {
            for (term, weight) in &draft.terms {
                *file_weights.entry(term).or_default() += weight;
                let in_path = if cached.path_terms.contains(term) { PATH_WEIGHT } else { 0.0 };
                postings.entry(term).or_default().push((next_chunk, whole(weight + in_path)));
            }
            for term in &cached.path_terms {
                if draft.terms.iter().all(|(known, _)| known != term) {
                    postings.entry(term).or_default().push((next_chunk, whole(PATH_WEIGHT)));
                }
            }
            let vector = (dimension > 0).then(|| quantize(&draft.vector));
            writer.chunk(
                draft.start_line,
                draft.end_line,
                draft.length,
                draft.imports,
                draft.names.iter().map(String::as_str).zip(draft.name_lines.iter().copied()),
                vector.as_ref().map(|(row, scale)| (row.as_slice(), *scale)),
            );
            next_chunk += 1;
        }
        for (term, weight) in file_weights {
            file_postings.entry(term).or_default().push((file, whole(weight)));
        }
    }
    let mut terms: Vec<&str> = postings.keys().chain(file_postings.keys()).copied().collect();
    terms.sort_unstable();
    terms.dedup();
    for term in terms {
        writer.term(
            term.as_bytes(),
            postings.get(term).map_or(&[][..], Vec::as_slice),
            file_postings.get(term).map_or(&[][..], Vec::as_slice),
        );
    }
    writer.finish(store::new_id())
}

/// A file read for its words, and with the model for its vectors when given.
///
/// A file that cannot be read as text, or is not worth reading, is remembered
/// as such: forgetting it would make every refresh see a tree that differs
/// from the store and read it again for nothing.
fn read_file(
    tokenizer: &Tokenizer,
    model: Option<&StaticModel>,
    relative: &str,
    path: &Path,
    kind: Kind,
    stamp: Stamp,
) -> CachedFile {
    let bytes = std::fs::read(path).unwrap_or_default();
    let hash = content_hash(&bytes);
    let text = String::from_utf8(bytes).ok().filter(|text| !is_minified(text));
    let Some(text) = text else {
        return CachedFile {
            stamp,
            walked: kind,
            kind,
            indexed: false,
            path_terms: Vec::new(),
            drafts: Vec::new(),
            hash,
        };
    };
    let mut path_terms = Vec::new();
    let mut tail: Vec<&str> = relative.rsplit('/').take(2).collect();
    tail.reverse();
    tokenizer.terms(&tail.join(" "), &mut path_terms);
    path_terms.sort();
    path_terms.dedup();
    let tests = kind == Kind::Code && !relative.ends_with(".rs") && reads_as_tests(&text);
    CachedFile {
        stamp,
        walked: kind,
        kind: if tests { Kind::Test } else { kind },
        indexed: true,
        path_terms,
        drafts: draft_file(tokenizer, model, relative, &text),
        hash,
    }
}

fn content_hash(bytes: &[u8]) -> [u8; 16] {
    let digest = Sha256::digest(bytes);
    let mut hash = [0u8; 16];
    hash.copy_from_slice(&digest[..16]);
    hash
}

fn borrowed(root: &Path, model_dir: Option<&Path>, cache_dir: &Path, walked: &Walked, missing: &[usize]) -> Vec<Vec<u8>> {
    let main = roots::repository(root);
    if missing.is_empty() || !main.join(".git").exists() {
        return Vec::new();
    }
    let checkouts = std::iter::once(main.clone()).chain(roots::linked_worktrees(&main).into_iter().map(|worktree| worktree.path));
    let donors: Vec<Arc<Segment>> = checkouts
        .filter(|checkout| checkout.as_path() != root)
        .filter_map(|checkout| store_path(cache_dir, &checkout, model_dir))
        .filter(|store| store.is_file())
        .flat_map(|store| store::open(&store).segments)
        .collect();
    if donors.is_empty() {
        return Vec::new();
    }
    let mut newest: HashMap<&str, (usize, usize)> = HashMap::new();
    for (part, segment) in donors.iter().enumerate().rev() {
        for local in 0..segment.file_count() {
            newest.entry(segment.file(local).path).or_insert((part, local));
        }
    }
    let found: Vec<(usize, usize, Stamp)> = missing
        .par_iter()
        .filter_map(|&at| {
            let (relative, path, kind, stamp) = &walked[at];
            let &(part, local) = newest.get(relative.as_str())?;
            let record = donors[part].file(local);
            (record.walked == *kind && record.hash == content_hash(&std::fs::read(path).ok()?)).then_some((part, local, *stamp))
        })
        .collect();
    let parts: Vec<&Arc<Segment>> = donors.iter().collect();
    let mut records = Vec::new();
    for embedded in [true, false] {
        let group: Vec<&(usize, usize, Stamp)> =
            found.iter().filter(|(part, ..)| donors[*part].vectors().is_some() == embedded).collect();
        if group.is_empty() {
            continue;
        }
        let dimension = group.first().and_then(|(part, ..)| donors[*part].vectors()).map_or(0, |vectors| vectors.dimension);
        let entries: Vec<(usize, usize)> = group.iter().map(|&&(part, local, _)| (part, local)).collect();
        let stamps: Vec<Stamp> = group.iter().map(|&&(.., stamp)| stamp).collect();
        records.push(merge(&parts, &entries, Some(&stamps), dimension));
    }
    records
}

/// The user's cache of stores.
fn cache_dir() -> Option<PathBuf> {
    crate::paths::stores()
}

/// One store per repository and model: vectors from one model mean nothing
/// to another, and a lexical-only store has none.
fn store_path(cache_dir: &Path, root: &Path, model_dir: Option<&Path>) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let model = model_dir.map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    let name: String = hasher.finalize().iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    Some(cache_dir.join(format!("{name}.idx")))
}

/// The cache of the same repository as an earlier release kept it.
fn forget_legacy(store: &Path) {
    let _ = std::fs::remove_file(store.with_extension("bin"));
}

fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    Stamp {
        modified: meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| (since.as_secs(), since.subsec_nanos())),
        bytes: meta.len(),
    }
}

fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.add_custom_ignore_filename(".omegaignore").require_git(false);
    builder
}

fn walked_entry(root: &Path, entry: ignore::DirEntry) -> Option<(String, PathBuf, Kind, Stamp)> {
    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
        return None;
    }
    let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
    if is_junk(relative) {
        return None;
    }
    let kind = kind_of(relative)?;
    let meta = entry.metadata().ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let relative = relative.to_string_lossy().replace('\\', "/");
    Some((relative, entry.into_path(), kind, stamp_of(&meta)))
}

fn walk(root: &Path) -> Walked {
    let found = std::sync::Mutex::new(Walked::new());
    walker(root).build_parallel().run(|| {
        Box::new(|entry| {
            if let Some(kept) = entry.ok().and_then(|entry| walked_entry(root, entry)) {
                if let Ok(mut found) = found.lock() {
                    found.push(kept);
                }
            }
            ignore::WalkState::Continue
        })
    });
    let mut walked = found.into_inner().unwrap_or_default();
    walked.sort_by(|a, b| a.0.cmp(&b.0));
    walked
}

fn walk_within(root: &Path, paths: &[PathBuf]) -> Walked {
    let wanted = paths.to_vec();
    let mut builder = walker(root);
    builder.filter_entry(move |entry| {
        let path = entry.path();
        wanted.iter().any(|changed| changed.starts_with(path) || path.starts_with(changed))
    });
    builder.build().filter_map(Result::ok).filter_map(|entry| walked_entry(root, entry)).collect()
}

fn draft_file(tokenizer: &Tokenizer, model: Option<&StaticModel>, relative: &str, text: &str) -> Vec<Draft> {
    let lines: Vec<&str> = text.lines().collect();
    let syntax = syntax_of(relative, &lines);
    let styled = styled_names(&lines, &syntax);
    chunk(&lines)
        .into_iter()
        .map(|span| {
            let body = lines[span.start..span.end].join("\n");
            let (names, name_lines) =
                names_in(
                &lines[span.start..span.end],
                &syntax[span.start..span.end],
                &styled[span.start..span.end],
                span.start as u32 + 1,
            );
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

/// What kind of text a line is, which decides what declaring looks like in it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Syntax {
    Code,
    /// CSS and its dialects: a rule declares its selector, and there are
    /// mixins, keyframes, variables and custom properties.
    Sheet,
    /// HTML outside its scripts and styles: an element with an `id` is what
    /// the rest of the code refers to.
    Markup,
    /// Markdown and its kin: a heading is what a document declares. A fenced
    /// block of code inside it is quoted, not declared: a README that shows
    /// `fn main` is not where `main` is defined.
    Prose,
    Quoted,
}

/// The syntax of every line of a file, from its extension and, where one file
/// holds several -- a page, a single-file component -- from its `<style>` and
/// `<script>` sections.
fn syntax_of(relative: &str, lines: &[&str]) -> Vec<Syntax> {
    let extension = relative.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    let outside = match extension.as_str() {
        "css" | "scss" | "less" | "pcss" | "postcss" => return vec![Syntax::Sheet; lines.len()],
        "html" | "htm" => Syntax::Markup,
        "vue" | "svelte" | "astro" => Syntax::Code,
        "md" | "mdx" | "markdown" | "rst" | "adoc" | "txt" => Syntax::Prose,
        _ => return vec![Syntax::Code; lines.len()],
    };
    let mut current = outside;
    // A fence opens a block of code and the next fence closes it.
    let mut fenced = false;
    lines
        .iter()
        .map(|line| {
            let line = line.trim_start();
            if outside == Syntax::Prose {
                if line.starts_with("```") || line.starts_with("~~~") {
                    fenced = !fenced;
                    return Syntax::Prose;
                }
                return if fenced { Syntax::Quoted } else { Syntax::Prose };
            }
            if line.starts_with("</style") || line.starts_with("</script") {
                current = outside;
            } else if line.starts_with("<style") && !line.contains("</style") {
                current = Syntax::Sheet;
                return outside;
            } else if line.starts_with("<script") && !line.contains("</script") {
                current = Syntax::Code;
                return outside;
            }
            current
        })
        .collect()
}

/// What a line of a stylesheet declares: the first class or id of a rule's
/// selector (else its element), a mixin, a function, keyframes, a placeholder,
/// a top-level variable, a custom property. Hyphens belong to these names.
fn declared_in_sheet(line: &str) -> Option<&str> {
    static AT_RULE: OnceLock<Regex> = OnceLock::new();
    static VARIABLE: OnceLock<Regex> = OnceLock::new();
    static HOOK: OnceLock<Regex> = OnceLock::new();
    static ELEMENT: OnceLock<Regex> = OnceLock::new();
    let at_rule = AT_RULE.get_or_init(|| {
        Regex::new(r"^@(?:mixin|function|keyframes|-webkit-keyframes)\s+([A-Za-z_][\w-]*)").expect("a valid regex")
    });
    // `$gap: 8px` where a file declares it, `--gap: 8px` wherever it is set.
    let variable = VARIABLE
        .get_or_init(|| Regex::new(r"^(?:(\$[A-Za-z_][\w-]*)|\s+(--[A-Za-z_][\w-]*)|(--[A-Za-z_][\w-]*))\s*:").expect("a valid regex"));
    let hook = HOOK.get_or_init(|| Regex::new(r"[.#%]([A-Za-z_][\w-]*)").expect("a valid regex"));
    // `*` and `*:focus-visible` are rules about everything, named `*`.
    let element = ELEMENT.get_or_init(|| Regex::new(r"^(?::{0,2}([A-Za-z][\w-]*)|(\*))").expect("a valid regex"));

    let trimmed = line.trim();
    // `* text` continues a comment; `*:focus-visible {` is a rule.
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("* ") || trimmed == "*" || trimmed.starts_with("*/") {
        return None;
    }
    if let Some(found) = at_rule.captures(trimmed) {
        return found.get(1).map(|name| name.as_str());
    }
    if let Some(found) = variable.captures(line) {
        return found.iter().skip(1).flatten().next().map(|name| name.as_str());
    }
    // A rule opens a block; `@media (...) {` and `@include x {` open one too
    // and declare nothing.
    if trimmed.starts_with('@') {
        return None;
    }
    // The selector is what precedes the brace, whether the rule goes on below
    // or is written out on this one line: `.chip { padding: 0 8px; }`.
    let (selector, _) = trimmed.split_once('{')?;
    let selector = selector.trim_end();
    // `font: {` is a nested property and `width: calc(#{$gap})` a value; a
    // selector has no `: ` in it, only `:hover`.
    if selector.is_empty() || selector.ends_with([':', '#']) || selector.contains(": ") || selector.contains(';') {
        return None;
    }
    hook.captures(selector)
        .or_else(|| element.captures(selector))
        .and_then(|found| found.get(1).or_else(|| found.get(2)))
        .map(|name| name.as_str())
        // The steps of an animation are not what a stylesheet declares.
        .filter(|name| !matches!(*name, "from" | "to"))
}

/// What each line of a file declares where that is not code: a stylesheet's
/// names, a page's ids. Read over the whole file, because a nested rule is
/// named by the rules it sits in: under `.card`, `&__title` declares
/// `card__title` -- the name the markup uses and no search by text can find.
fn styled_names(lines: &[&str], syntax: &[Syntax]) -> Vec<Option<String>> {
    // The rules a line sits in, by indentation, innermost last.
    let mut within: Vec<(usize, String)> = Vec::new();
    lines
        .iter()
        .zip(syntax)
        .map(|(line, syntax)| match syntax {
            Syntax::Code => None,
            Syntax::Markup => declared_in_markup(line).map(str::to_owned),
            Syntax::Prose => declared_in_prose(line).map(str::to_owned),
            Syntax::Quoted => None,
            Syntax::Sheet => {
                let trimmed = line.trim_start();
                if trimmed.is_empty() {
                    return None;
                }
                let indent = line.len() - trimmed.len();
                while within.last().is_some_and(|(at, _)| *at >= indent) {
                    within.pop();
                }
                let opens = trimmed.trim_end().ends_with('{');
                let name = match trimmed.strip_prefix('&') {
                    Some(rest) if opens => {
                        let suffix: String =
                            rest.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-')).collect();
                        match within.last() {
                            Some((_, parent)) if !suffix.is_empty() => Some(format!("{parent}{suffix}")),
                            _ => None,
                        }
                    }
                    Some(_) => None,
                    None => declared_in_sheet(line).map(str::to_owned),
                };
                if opens {
                    // `&:hover` and `@media` name nothing, and what is nested
                    // in them still belongs to the rule around them.
                    let carried = name.clone().or_else(|| within.last().map(|(_, name)| name.clone()));
                    if let Some(carried) = carried {
                        within.push((indent, carried));
                    }
                }
                name
            }
        })
        .collect()
}

/// The `id` an element is given, which is how scripts and styles refer to it.
fn declared_in_markup(line: &str) -> Option<&str> {
    static ID: OnceLock<Regex> = OnceLock::new();
    static TEMPLATE: OnceLock<Regex> = OnceLock::new();
    // A named template -- Go's `{{define "x"}}`, Jinja's and Twig's
    // `{% block x %}` and `{% macro x(`, Django's too -- is what the other
    // templates and the code refer to, by that name.
    let template = TEMPLATE.get_or_init(|| {
        Regex::new(r#"\{\{-?\s*(?:define|block)\s+"([^"]+)"|\{%-?\s*(?:block|macro)\s+([A-Za-z_][\w-]*)"#).expect("a valid regex")
    });
    if let Some(found) = template.captures(line) {
        return found.get(1).or_else(|| found.get(2)).map(|name| name.as_str());
    }
    // An id is written in whatever alphabet the page is.
    let id = ID.get_or_init(|| Regex::new(r#"\sid=["']([\p{L}_][\p{L}\p{N}_-]*)["']"#).expect("a valid regex"));
    id.captures(line).and_then(|found| found.get(1)).map(|name| name.as_str())
}

/// A heading, as a document's own name for the section it opens: `## Setup`
/// declares `Setup`, with its marks, links and trailing hashes taken off.
fn declared_in_prose(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('#')?;
    let level = 1 + rest.chars().take_while(|&c| c == '#').count();
    if level > 6 {
        return None;
    }
    let title = rest.trim_start_matches('#');
    // `#hashtag` and `#[derive]` are not headings; a heading has a space.
    if !title.starts_with([' ', '\t']) {
        return None;
    }
    let title = title.trim().trim_end_matches('#').trim();
    (!title.is_empty()).then_some(title)
}

/// The identifiers a run of lines declares, and the line of each.
fn names_in(
    lines: &[&str],
    syntax: &[Syntax],
    styled: &[Option<String>],
    first_line: u32,
) -> (Vec<String>, Vec<u32>) {
    let mut names: Vec<String> = Vec::new();
    let mut name_lines = Vec::new();
    for (offset, line) in lines.iter().enumerate() {
        if syntax.get(offset).is_some_and(|syntax| *syntax != Syntax::Code) {
            // A stylesheet says `.card__icon` again under each modifier, and
            // every place it does is a place the agent has to know about.
            if let Some(name) = styled.get(offset).and_then(Option::as_ref) {
                names.push(name.clone());
                name_lines.push(first_line + offset as u32);
                if names.len() == MAX_NAMES {
                    break;
                }
            }
            continue;
        }
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
/// A bundle: long lines throughout. A page whose lines are long because each
/// holds an inline SVG or a one-line template is not one -- it still breaks
/// where a person broke it, so the longest run of long lines is short.
fn is_minified(text: &str) -> bool {
    let lines = text.lines().count().max(1);
    if text.len() <= 4096 || text.len() / lines <= 400 {
        return false;
    }
    let (mut run, mut longest) = (0, 0);
    for line in text.lines() {
        run = if line.len() > 400 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    lines < 8 || longest * 2 > lines
}

fn kind_of(relative: &Path) -> Option<Kind> {
    let extension = relative.extension()?.to_str()?.to_ascii_lowercase();
    let kind = match extension.as_str() {
        "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" | "java" | "kt" | "kts"
        | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "cs" | "rb" | "php" | "swift" | "scala"
        | "sh" | "bash" | "ps1" | "sql" | "lua" | "dart" | "ex" | "exs" | "vue" | "svelte" | "astro"
        | "zig" | "hs" | "ml" | "clj" | "r" | "m" | "mm" | "pl" | "proto" | "graphql" | "css"
        | "scss" | "sass" | "less" | "styl" | "pcss" | "postcss" | "html" | "htm" => Kind::Code,
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
        assert_eq!(names_in(&definition, &[], &[], 314), (vec!["kt_find_extension_receiver".to_owned()], vec![314]));
        // The same, ending in `;`, is a prototype and declares nothing here.
        let prototype = ["int cbm_store_upsert(cbm_store_t *store, const cbm_node_t *node,", "                     int flags);"];
        assert_eq!(names_in(&prototype, &[], &[], 1).0, Vec::<String>::new());
    }

    #[test]
    fn what_a_stylesheet_and_a_page_declare() {
        let sheet = [
            "$grid-gap: 8px;",                       // 1
            "@mixin truncate($lines) {",             // 2
            "  overflow: hidden;",                   // 3
            "}",                                     // 4
            ".card {",                               // 5
            "  --card-radius: 4px;",                 // 6
            "  &__title {",                          // 7
            "    font: {",                           // 8
            "      weight: 600;",                    // 9
            "    }",                                 // 10
            "    &--active {",                       // 11
            "      color: red;",                     // 12
            "    }",                                 // 13
            "  }",                                   // 14
            "  &:hover {",                           // 15
            "    .card__icon, .other {",             // 16
            "    }",                                 // 17
            "  }",                                   // 18
            "  @media (min-width: 600px) {",         // 19
            "    &__body {",                         // 20
            "    }",                                 // 21
            "  }",                                   // 22
            "}",                                     // 23
            "@keyframes fade-in {",                  // 24
            "  from {",                              // 25
            "  }",                                   // 26
            "}",                                     // 27
            ":root {",                               // 28
            "#site-header > nav {",                  // 29
            "  width: calc(#{$grid-gap} * 2);",      // 30
            "}",                                     // 31
            ".chip { padding: 0 8px; }",             // 32
            "*:focus-visible {",                     // 33
            "  outline: 2px solid red;",             // 34
            "}",                                     // 35
        ];
        let syntax = super::syntax_of("theme/card.scss", &sheet);
        let styled = super::styled_names(&sheet, &syntax);
        let (names, lines) = names_in(&sheet, &syntax, &styled, 1);
        let declared: Vec<(&str, u32)> = names.iter().map(String::as_str).zip(lines).collect();
        assert_eq!(
            declared,
            [
                ("$grid-gap", 1),
                ("truncate", 2),
                ("card", 5),
                ("--card-radius", 6),
                ("card__title", 7),
                ("card__title--active", 11),
                ("card__icon", 16),
                ("card__body", 20),
                ("fade-in", 24),
                ("root", 28),
                ("site-header", 29),
                ("chip", 32),
                ("*", 33),
            ]
        );

        // A component is code, except between its style tags; a page is markup
        // outside its scripts and styles.
        let component = ["<script setup>", "const open = ref(false)", "</script>", "<style scoped>", ".menu {", "}", "</style>"];
        let syntax = super::syntax_of("Menu.vue", &component);
        let styled = super::styled_names(&component, &syntax);
        assert_eq!(names_in(&component, &syntax, &styled, 1).0, ["open", "menu"]);
        let page = ["{{define \"page-head\"}}", "<main id=\"app\">", "<script>", "function boot() {", "}", "</script>", "</main>", "{% block content %}"];
        let syntax = super::syntax_of("index.html", &page);
        let styled = super::styled_names(&page, &syntax);
        assert_eq!(names_in(&page, &syntax, &styled, 1).0, ["page-head", "app", "boot", "content"]);
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
