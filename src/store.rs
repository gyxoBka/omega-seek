//! The index on disk: one file per repository and model, a run of segments.
//!
//! A segment is a whole index of some files -- their chunks and names, a term
//! dictionary with postings, and optionally their vectors -- laid out to be
//! read where it lies, from a mapping of the file, rather than decoded into
//! memory first. A start therefore costs what the tables of files and chunks
//! cost, not what the postings and vectors weigh.
//!
//! The file is only ever appended to, or replaced whole by a file written
//! beside it and renamed over; never truncated or written in place. A process
//! that mapped it keeps reading what it mapped, and a segment cut short by a
//! crash is told by its missing trailer and read no further.
//!
//! Terms are stored once per segment, postings as varint gaps with their
//! weights, and vectors as bytes with one scale per row: a quarter of what
//! floats weigh, for a ranking that is read by rank anyway.

use crate::index::Kind;
use memmap2::Mmap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const MAGIC: &[u8; 8] = b"OMEGAIDX";
const TRAILER: &[u8; 8] = b"XDIAGEMO";
/// Magic, version, kind, payload length.
const HEADER: usize = 24;
/// Payload length again, then the trailer.
const FOOTER: usize = 16;
/// Bumped whenever what a file is read as changes -- chunking, tokenizing,
/// naming, embedding -- or the layout does: an older segment is then skipped
/// rather than trusted, and the file is rewritten.
pub const VERSION: u32 = 16;

const LEXICAL: u32 = 1;
const VECTORS: u32 = 2;

const STRINGS: usize = 0;
const FILES: usize = 1;
const CHUNKS: usize = 2;
const NAMES: usize = 3;
const TERMS: usize = 4;
const POSTINGS: usize = 5;
const FILE_POSTINGS: usize = 6;
const VECS: usize = 7;
const SECTIONS: usize = 8;
/// The id, then an offset and a length for each section.
const PREFIX: usize = 8 + SECTIONS * 16;

const FILE_RECORD: usize = 44;
const CHUNK_RECORD: usize = 24;
const NAME_RECORD: usize = 12;
const TERM_RECORD: usize = 32;
const VECTORS_PREFIX: usize = 16;

/// What stands for "this file as it was when read".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stamp {
    /// Seconds and nanoseconds since the epoch.
    pub modified: Option<(u64, u32)>,
    pub bytes: u64,
}

fn kind_byte(kind: Kind) -> u8 {
    match kind {
        Kind::Code => 0,
        Kind::Test => 1,
        Kind::Docs => 2,
        Kind::Config => 3,
    }
}

fn byte_kind(byte: u8) -> Kind {
    match byte {
        1 => Kind::Test,
        2 => Kind::Docs,
        3 => Kind::Config,
        _ => Kind::Code,
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn f32_at(bytes: &[u8], at: usize) -> f32 {
    f32::from_bits(u32_at(bytes, at))
}

fn push_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

/// A row as bytes and the scale that brings it back: `value = byte * scale`.
#[must_use]
pub fn quantize(row: &[f32]) -> (Vec<u8>, f32) {
    let largest = row.iter().fold(0.0f32, |largest, value| largest.max(value.abs()));
    if largest == 0.0 || !largest.is_finite() {
        return (vec![0; row.len()], 0.0);
    }
    let scale = largest / 127.0;
    let bytes = row.iter().map(|value| (value / scale).round().clamp(-127.0, 127.0) as i8 as u8).collect();
    (bytes, scale)
}

fn dot(row: &[u8], asked: &[f32]) -> f32 {
    row.iter().zip(asked).map(|(&byte, asked)| f32::from(byte as i8) * asked).sum()
}

/// Where a segment's bytes live: a mapping of the store, or memory for a
/// segment that was just built.
#[derive(Clone)]
pub enum Bytes {
    Mapped(Arc<Mmap>),
    Owned(Arc<Vec<u8>>),
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({})", self.len())
    }
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped(map) => map,
            Self::Owned(bytes) => bytes,
        }
    }
}

/// One file of a segment, as it was read.
#[derive(Clone, Copy, Debug)]
pub struct FileRecord<'a> {
    pub path: &'a str,
    pub stamp: Stamp,
    /// What the path says the file is: what the walk compares against.
    pub walked: Kind,
    /// What it is treated as, which its contents may have changed.
    pub kind: Kind,
    /// False for a file that was looked at and left out.
    pub indexed: bool,
    pub chunk_start: u32,
    pub chunk_count: u32,
    /// Its chunks' lengths in terms, together.
    pub length: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct ChunkRecord {
    pub file: u32,
    pub start_line: u32,
    pub end_line: u32,
    pub length: f32,
    pub imports: bool,
    names_start: u32,
    names_count: u32,
}

#[derive(Clone, Copy, Debug)]
struct TermRecord {
    postings: usize,
    postings_count: u32,
    file_postings: usize,
    file_postings_count: u32,
}

/// The vectors of a segment's chunks and files, one byte per dimension.
pub struct Vectors {
    bytes: Bytes,
    pub dimension: usize,
    chunks: usize,
    files: usize,
    chunk_scales: usize,
    file_scales: usize,
    chunk_rows: usize,
    file_rows: usize,
}

impl std::fmt::Debug for Vectors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vectors")
            .field("dimension", &self.dimension)
            .field("chunks", &self.chunks)
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

impl Vectors {
    /// A vectors block at `at..at + length`, checked to fit.
    fn parse(bytes: Bytes, at: usize, length: usize) -> Option<Self> {
        if length < VECTORS_PREFIX {
            return None;
        }
        let dimension = u32_at(&bytes, at) as usize;
        let chunks = u32_at(&bytes, at + 4) as usize;
        let files = u32_at(&bytes, at + 8) as usize;
        let chunk_scales = at + VECTORS_PREFIX;
        let file_scales = chunk_scales + chunks * 4;
        let chunk_rows = file_scales + files * 4;
        let file_rows = chunk_rows + chunks * dimension;
        (dimension > 0 && file_rows + files * dimension == at + length).then_some(Self {
            bytes,
            dimension,
            chunks,
            files,
            chunk_scales,
            file_scales,
            chunk_rows,
            file_rows,
        })
    }

    #[must_use]
    pub fn chunk_dot(&self, chunk: usize, asked: &[f32]) -> f32 {
        let (row, scale) = self.chunk_row(chunk);
        dot(row, asked) * scale
    }

    #[must_use]
    pub fn file_dot(&self, file: usize, asked: &[f32]) -> f32 {
        let (row, scale) = self.file_row(file);
        dot(row, asked) * scale
    }

    #[must_use]
    pub fn chunk_row(&self, chunk: usize) -> (&[u8], f32) {
        let start = self.chunk_rows + chunk * self.dimension;
        (&self.bytes[start..start + self.dimension], f32_at(&self.bytes, self.chunk_scales + chunk * 4))
    }

    #[must_use]
    pub fn file_row(&self, file: usize) -> (&[u8], f32) {
        let start = self.file_rows + file * self.dimension;
        (&self.bytes[start..start + self.dimension], f32_at(&self.bytes, self.file_scales + file * 4))
    }
}

/// A segment read where it lies.
pub struct Segment {
    bytes: Bytes,
    pub id: u64,
    sections: [(usize, usize); SECTIONS],
    files: usize,
    chunks: usize,
    terms: usize,
    /// Inline, or attached by a later record once they are computed.
    vectors: OnceLock<Vectors>,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("id", &self.id)
            .field("files", &self.files)
            .field("chunks", &self.chunks)
            .field("vectors", &self.vectors.get().is_some())
            .finish_non_exhaustive()
    }
}

impl Segment {
    /// A segment just built, held in memory.
    pub fn owned(bytes: Vec<u8>) -> Option<Arc<Self>> {
        let length = bytes.len();
        let bytes = Bytes::Owned(Arc::new(bytes));
        let (kind, payload, end) = frame(&bytes, 0)?;
        (kind == LEXICAL && end == length).then(|| Self::parse(bytes.clone(), payload, end - FOOTER - payload))?
    }

    /// The lexical payload at `at..at + length`, every table checked to fit
    /// and every reference between tables checked to land: a store written by
    /// something else, or cut short, is refused rather than read out of bounds.
    fn parse(bytes: Bytes, at: usize, length: usize) -> Option<Arc<Self>> {
        if length < PREFIX {
            return None;
        }
        let id = u64_at(&bytes, at);
        let mut sections = [(0usize, 0usize); SECTIONS];
        for (index, section) in sections.iter_mut().enumerate() {
            let offset = usize::try_from(u64_at(&bytes, at + 8 + index * 16)).ok()?;
            let size = usize::try_from(u64_at(&bytes, at + 16 + index * 16)).ok()?;
            if offset.checked_add(size)? > length {
                return None;
            }
            *section = (at + offset, size);
        }
        let count = |section: usize, record: usize| {
            let size = sections[section].1;
            (size % record == 0).then_some(size / record)
        };
        let files = count(FILES, FILE_RECORD)?;
        let chunks = count(CHUNKS, CHUNK_RECORD)?;
        let names = count(NAMES, NAME_RECORD)?;
        let terms = count(TERMS, TERM_RECORD)?;
        let segment = Self {
            bytes: bytes.clone(),
            id,
            sections,
            files,
            chunks,
            terms,
            vectors: OnceLock::new(),
        };
        for file in 0..files {
            let record = segment.file_raw(file);
            let chunk_end = u64::from(record.chunk_start) + u64::from(record.chunk_count);
            if chunk_end > chunks as u64 {
                return None;
            }
        }
        for chunk in 0..chunks {
            let record = segment.chunk(chunk);
            if record.file as usize >= files || record.names_start as usize + record.names_count as usize > names {
                return None;
            }
        }
        let (vectors_at, vectors_length) = sections[VECS];
        if vectors_length > 0 {
            let vectors = Vectors::parse(bytes, vectors_at, vectors_length)?;
            if vectors.chunks != chunks || vectors.files != files {
                return None;
            }
            let _ = segment.vectors.set(vectors);
        }
        Some(Arc::new(segment))
    }

    fn record(&self, section: usize, size: usize, index: usize) -> usize {
        self.sections[section].0 + index * size
    }

    fn string(&self, at: usize) -> &[u8] {
        let offset = u32_at(&self.bytes, at) as usize;
        let length = u32_at(&self.bytes, at + 4) as usize;
        let (start, size) = self.sections[STRINGS];
        if offset.saturating_add(length) > size {
            return &[];
        }
        &self.bytes[start + offset..start + offset + length]
    }

    fn text(&self, at: usize) -> &str {
        std::str::from_utf8(self.string(at)).unwrap_or_default()
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files
    }

    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks
    }

    fn file_raw(&self, index: usize) -> FileRecord<'_> {
        let at = self.record(FILES, FILE_RECORD, index);
        let bytes = &self.bytes;
        let nanos = u32_at(bytes, at + 16);
        FileRecord {
            path: self.text(at),
            stamp: Stamp {
                modified: (nanos != u32::MAX).then(|| (u64_at(bytes, at + 8), nanos)),
                bytes: u64_at(bytes, at + 20),
            },
            walked: byte_kind(bytes[at + 28]),
            kind: byte_kind(bytes[at + 29]),
            indexed: bytes[at + 30] != 0,
            chunk_start: u32_at(bytes, at + 32),
            chunk_count: u32_at(bytes, at + 36),
            length: f32_at(bytes, at + 40),
        }
    }

    #[must_use]
    pub fn file(&self, index: usize) -> FileRecord<'_> {
        self.file_raw(index)
    }

    #[must_use]
    pub fn chunk(&self, index: usize) -> ChunkRecord {
        let at = self.record(CHUNKS, CHUNK_RECORD, index);
        let bytes = &self.bytes;
        ChunkRecord {
            file: u32_at(bytes, at),
            start_line: u32_at(bytes, at + 4),
            end_line: u32_at(bytes, at + 8),
            length: f32_at(bytes, at + 12),
            names_start: u32_at(bytes, at + 16),
            names_count: u32::from(u16::from_le_bytes([bytes[at + 20], bytes[at + 21]])),
            imports: bytes[at + 22] != 0,
        }
    }

    /// What a chunk declares, in source order, and the line of each.
    pub fn names(&self, chunk: &ChunkRecord) -> impl Iterator<Item = (&str, u32)> {
        (chunk.names_start..chunk.names_start + chunk.names_count).map(move |name| {
            let at = self.record(NAMES, NAME_RECORD, name as usize);
            (self.text(at), u32_at(&self.bytes, at + 8))
        })
    }

    #[must_use]
    pub fn term_count(&self) -> usize {
        self.terms
    }

    /// The term at `index` of the dictionary, which is in byte order.
    #[must_use]
    pub fn term(&self, index: usize) -> &[u8] {
        self.string(self.record(TERMS, TERM_RECORD, index))
    }

    fn term_record(&self, index: usize) -> TermRecord {
        let at = self.record(TERMS, TERM_RECORD, index);
        let bytes = &self.bytes;
        TermRecord {
            postings: u64_at(bytes, at + 8) as usize,
            postings_count: u32_at(bytes, at + 16),
            file_postings: u64_at(bytes, at + 20) as usize,
            file_postings_count: u32_at(bytes, at + 28),
        }
    }

    fn find(&self, term: &str) -> Option<usize> {
        let (mut low, mut high) = (0, self.terms);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.term(middle).cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    /// Each chunk the term occurs in, with its weight there.
    pub fn postings(&self, term: &str, mut each: impl FnMut(u32, u32)) {
        if let Some(found) = self.find(term) {
            let record = self.term_record(found);
            self.decode(POSTINGS, record.postings, record.postings_count, &mut each);
        }
    }

    /// Each file the term occurs in, with its weight there.
    pub fn file_postings(&self, term: &str, mut each: impl FnMut(u32, u32)) {
        if let Some(found) = self.find(term) {
            let record = self.term_record(found);
            self.decode(FILE_POSTINGS, record.file_postings, record.file_postings_count, &mut each);
        }
    }

    /// The postings of the term at `index` of the dictionary.
    pub fn postings_at(&self, index: usize, mut each: impl FnMut(u32, u32)) {
        let record = self.term_record(index);
        self.decode(POSTINGS, record.postings, record.postings_count, &mut each);
    }

    pub fn file_postings_at(&self, index: usize, mut each: impl FnMut(u32, u32)) {
        let record = self.term_record(index);
        self.decode(FILE_POSTINGS, record.file_postings, record.file_postings_count, &mut each);
    }

    /// The whole record of a segment built in memory, ready to append.
    #[must_use]
    pub fn owned_record(&self) -> Option<&[u8]> {
        match &self.bytes {
            Bytes::Owned(bytes) => Some(bytes),
            Bytes::Mapped(_) => None,
        }
    }

    fn decode(&self, section: usize, offset: usize, count: u32, each: &mut impl FnMut(u32, u32)) {
        let (start, size) = self.sections[section];
        let Some(bytes) = self.bytes.get(start..start + size) else {
            return;
        };
        let mut at = offset;
        let mut id = 0u32;
        for _ in 0..count {
            let (Some(gap), Some(weight)) = (read_varint(bytes, &mut at), read_varint(bytes, &mut at)) else {
                return;
            };
            id = id.wrapping_add(gap);
            each(id, weight);
        }
    }

    #[must_use]
    pub fn vectors(&self) -> Option<&Vectors> {
        self.vectors.get()
    }

    /// Hands over vectors computed after the segment was written.
    pub fn attach(&self, vectors: Vectors) -> bool {
        vectors.chunks == self.chunks && vectors.files == self.files && self.vectors.set(vectors).is_ok()
    }
}

/// The header and footer of the record at `at`: its kind, where its payload
/// starts, and where the record ends. None when it is not whole.
fn frame(bytes: &[u8], at: usize) -> Option<(u32, usize, usize)> {
    let header = bytes.get(at..at.checked_add(HEADER)?)?;
    if &header[..8] != MAGIC || u32_at(header, 8) != VERSION {
        return None;
    }
    let kind = u32_at(header, 12);
    let length = usize::try_from(u64_at(header, 16)).ok()?;
    let payload = at + HEADER;
    let end = payload.checked_add(length)?.checked_add(FOOTER)?;
    let footer = bytes.get(end - FOOTER..end)?;
    (u64_at(footer, 0) as usize == length && &footer[8..] == TRAILER).then_some((kind, payload, end))
}

fn framed(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len() + FOOTER);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(TRAILER);
    out
}

/// A segment's vectors as rows of bytes, filled chunk by chunk and file by file.
#[derive(Debug)]
pub struct VectorRows {
    dimension: usize,
    chunk_scales: Vec<u8>,
    file_scales: Vec<u8>,
    chunk_rows: Vec<u8>,
    file_rows: Vec<u8>,
}

impl VectorRows {
    #[must_use]
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension,
            chunk_scales: Vec::new(),
            file_scales: Vec::new(),
            chunk_rows: Vec::new(),
            file_rows: Vec::new(),
        }
    }

    fn push(rows: &mut Vec<u8>, scales: &mut Vec<u8>, dimension: usize, row: &[u8], scale: f32) {
        let start = rows.len();
        rows.extend_from_slice(&row[..row.len().min(dimension)]);
        rows.resize(start + dimension, 0);
        scales.extend_from_slice(&scale.to_le_bytes());
    }

    pub fn chunk(&mut self, row: &[u8], scale: f32) {
        Self::push(&mut self.chunk_rows, &mut self.chunk_scales, self.dimension, row, scale);
    }

    pub fn file(&mut self, row: &[u8], scale: f32) {
        Self::push(&mut self.file_rows, &mut self.file_scales, self.dimension, row, scale);
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.dimension as u32).to_le_bytes());
        out.extend_from_slice(&((self.chunk_scales.len() / 4) as u32).to_le_bytes());
        out.extend_from_slice(&((self.file_scales.len() / 4) as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.chunk_scales);
        out.extend_from_slice(&self.file_scales);
        out.extend_from_slice(&self.chunk_rows);
        out.extend_from_slice(&self.file_rows);
    }

    /// The vectors of the segment `target`, as a record of their own.
    #[must_use]
    pub fn attachment(&self, target: u64) -> Vec<u8> {
        let mut payload = target.to_le_bytes().to_vec();
        self.encode(&mut payload);
        framed(VECTORS, &payload)
    }

    /// The same, readable in memory, for handing to the segment at once.
    #[must_use]
    pub fn into_vectors(self) -> Option<Vectors> {
        let mut bytes = Vec::new();
        self.encode(&mut bytes);
        let length = bytes.len();
        Vectors::parse(Bytes::Owned(Arc::new(bytes)), 0, length)
    }
}

/// Builds a segment: files in path order, each followed by its chunks, then
/// terms in byte order.
#[derive(Debug)]
pub struct Writer {
    strings: Vec<u8>,
    files: Vec<u8>,
    chunks: Vec<u8>,
    names: Vec<u8>,
    terms: Vec<u8>,
    postings: Vec<u8>,
    file_postings: Vec<u8>,
    vectors: Option<VectorRows>,
    chunk_count: u32,
    name_count: u32,
    /// Where the chunk count of the file being written sits.
    open_file: Option<usize>,
}

impl Writer {
    /// With `dimension` above zero, every file and chunk comes with a row.
    #[must_use]
    pub fn new(dimension: usize) -> Self {
        Self {
            strings: Vec::new(),
            files: Vec::new(),
            chunks: Vec::new(),
            names: Vec::new(),
            terms: Vec::new(),
            postings: Vec::new(),
            file_postings: Vec::new(),
            vectors: (dimension > 0).then(|| VectorRows::new(dimension)),
            chunk_count: 0,
            name_count: 0,
            open_file: None,
        }
    }

    fn string(&mut self, out: Section, text: &[u8]) {
        let offset = self.strings.len() as u32;
        self.strings.extend_from_slice(text);
        let target = match out {
            Section::Files => &mut self.files,
            Section::Names => &mut self.names,
            Section::Terms => &mut self.terms,
        };
        target.extend_from_slice(&offset.to_le_bytes());
        target.extend_from_slice(&(text.len() as u32).to_le_bytes());
    }

    /// A file; its chunks follow. `vector` is its row when the segment has vectors.
    pub fn file(&mut self, record: &FileRecord<'_>, vector: Option<(&[u8], f32)>) {
        self.string(Section::Files, record.path.as_bytes());
        let (seconds, nanos) = record.stamp.modified.unwrap_or((0, u32::MAX));
        self.files.extend_from_slice(&seconds.to_le_bytes());
        self.files.extend_from_slice(&nanos.to_le_bytes());
        self.files.extend_from_slice(&record.stamp.bytes.to_le_bytes());
        self.files.extend_from_slice(&[kind_byte(record.walked), kind_byte(record.kind), u8::from(record.indexed), 0]);
        self.files.extend_from_slice(&self.chunk_count.to_le_bytes());
        self.open_file = Some(self.files.len());
        self.files.extend_from_slice(&0u32.to_le_bytes());
        self.files.extend_from_slice(&record.length.to_le_bytes());
        if let Some(rows) = &mut self.vectors {
            let (row, scale) = vector.unwrap_or((&[], 0.0));
            rows.file(row, scale);
        }
    }

    /// A chunk of the file last written.
    #[allow(clippy::too_many_arguments)]
    pub fn chunk<'a>(
        &mut self,
        start_line: u32,
        end_line: u32,
        length: f32,
        imports: bool,
        names: impl Iterator<Item = (&'a str, u32)>,
        vector: Option<(&[u8], f32)>,
    ) {
        let file = (self.files.len() / FILE_RECORD).saturating_sub(1) as u32;
        let names_start = self.name_count;
        let mut count = 0u16;
        for (name, line) in names {
            if count == u16::MAX {
                break;
            }
            self.string(Section::Names, name.as_bytes());
            self.names.extend_from_slice(&line.to_le_bytes());
            count += 1;
        }
        self.name_count += u32::from(count);
        self.chunks.extend_from_slice(&file.to_le_bytes());
        self.chunks.extend_from_slice(&start_line.to_le_bytes());
        self.chunks.extend_from_slice(&end_line.to_le_bytes());
        self.chunks.extend_from_slice(&length.to_le_bytes());
        self.chunks.extend_from_slice(&names_start.to_le_bytes());
        self.chunks.extend_from_slice(&count.to_le_bytes());
        self.chunks.extend_from_slice(&[u8::from(imports), 0]);
        self.chunk_count += 1;
        if let Some(at) = self.open_file {
            let written = u32_at(&self.files, at) + 1;
            self.files[at..at + 4].copy_from_slice(&written.to_le_bytes());
        }
        if let Some(rows) = &mut self.vectors {
            let (row, scale) = vector.unwrap_or((&[], 0.0));
            rows.chunk(row, scale);
        }
    }

    /// A term, in byte order after the last one, with its postings in
    /// ascending order of chunk and of file.
    pub fn term(&mut self, term: &[u8], postings: &[(u32, u32)], file_postings: &[(u32, u32)]) {
        self.string(Section::Terms, term);
        let encode = |out: &mut Vec<u8>, postings: &[(u32, u32)]| {
            let offset = out.len() as u64;
            let mut last = 0u32;
            for &(id, weight) in postings {
                push_varint(out, id - last);
                push_varint(out, weight);
                last = id;
            }
            offset
        };
        let offset = encode(&mut self.postings, postings);
        self.terms.extend_from_slice(&offset.to_le_bytes());
        self.terms.extend_from_slice(&(postings.len() as u32).to_le_bytes());
        let offset = encode(&mut self.file_postings, file_postings);
        self.terms.extend_from_slice(&offset.to_le_bytes());
        self.terms.extend_from_slice(&(file_postings.len() as u32).to_le_bytes());
    }

    /// The segment, framed, ready to append.
    #[must_use]
    pub fn finish(self, id: u64) -> Vec<u8> {
        let mut vectors = Vec::new();
        if let Some(rows) = &self.vectors {
            rows.encode(&mut vectors);
        }
        let sections: [&[u8]; SECTIONS] = [
            &self.strings,
            &self.files,
            &self.chunks,
            &self.names,
            &self.terms,
            &self.postings,
            &self.file_postings,
            &vectors,
        ];
        let mut payload = Vec::with_capacity(PREFIX + sections.iter().map(|section| section.len()).sum::<usize>());
        payload.extend_from_slice(&id.to_le_bytes());
        let mut offset = PREFIX as u64;
        for section in sections {
            payload.extend_from_slice(&offset.to_le_bytes());
            payload.extend_from_slice(&(section.len() as u64).to_le_bytes());
            offset += section.len() as u64;
        }
        for section in sections {
            payload.extend_from_slice(section);
        }
        framed(LEXICAL, &payload)
    }
}

#[derive(Clone, Copy)]
enum Section {
    Files,
    Names,
    Terms,
}

/// A fresh segment id: unique enough that an attachment cannot land on the
/// wrong segment of the same file.
#[must_use]
pub fn new_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    let mixed = nanos ^ (u64::from(std::process::id()) << 40) ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(20);
    // One round of splitmix, so neighbours differ in every bit.
    let mut z = mixed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The segments of a store file, as far as they are whole.
#[derive(Debug, Default)]
pub struct Opened {
    pub segments: Vec<Arc<Segment>>,
    /// Where the last whole record ends; past it is nothing, or a record cut
    /// short, or something that is not a store.
    pub clean_end: usize,
    pub length: usize,
}

impl Opened {
    /// Whether appending would put the new record where it can be found.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.clean_end == self.length
    }
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    // Let the file be renamed over and removed while mapped, as on Unix.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0x1 | 0x2 | 0x4);
    }
    options
}

#[allow(unsafe_code)]
fn map(file: &File) -> std::io::Result<Mmap> {
    // SAFETY: a store file is only ever appended to, or replaced by renaming
    // another file over it; nothing truncates it or writes it in place. The
    // bytes mapped are therefore never changed under the mapping, and a
    // replaced file stays readable to whoever mapped it.
    unsafe { Mmap::map(file) }
}

/// The store at `path`, mapped. A missing or unreadable file is an empty store.
#[must_use]
pub fn open(path: &Path) -> Opened {
    let Ok(file) = options().read(true).open(path) else {
        return Opened::default();
    };
    let length = file.metadata().map_or(0, |meta| meta.len() as usize);
    if length == 0 {
        return Opened::default();
    }
    let Ok(mapped) = map(&file) else {
        return Opened { length, ..Opened::default() };
    };
    let bytes = Bytes::Mapped(Arc::new(mapped));
    let length = bytes.len();
    let mut segments: Vec<Arc<Segment>> = Vec::new();
    let mut attachments: Vec<(u64, Vectors)> = Vec::new();
    let mut at = 0;
    while let Some((kind, payload, end)) = frame(&bytes, at) {
        let size = end - FOOTER - payload;
        match kind {
            LEXICAL => match Segment::parse(bytes.clone(), payload, size) {
                Some(segment) => segments.push(segment),
                None => break,
            },
            VECTORS if size >= 8 => match Vectors::parse(bytes.clone(), payload + 8, size - 8) {
                Some(vectors) => attachments.push((u64_at(&bytes, payload), vectors)),
                None => break,
            },
            _ => break,
        }
        at = end;
    }
    for (target, vectors) in attachments {
        if let Some(segment) = segments.iter().find(|segment| segment.id == target) {
            segment.attach(vectors);
        }
    }
    Opened { segments, clean_end: at, length }
}

/// Adds a record at the end of the store. A store that does not end on a
/// whole record -- cut short by a crash, or not a store -- is rewritten
/// instead, keeping what was whole, so the record can be found.
pub fn append(path: &Path, record: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !ends_whole(path) {
        let opened = open(path);
        let kept = open_prefix(path, opened.clean_end)?;
        drop(opened);
        return replace(path, &[&kept, record]);
    }
    let mut file = options().append(true).create(true).open(path)?;
    // One write: appends from other processes land before or after it whole.
    file.write_all(record)
}

/// Whether the file is missing, empty, or ends on a record's trailer.
fn ends_whole(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = options().read(true).open(path) else {
        return true;
    };
    let length = file.metadata().map_or(0, |meta| meta.len());
    if length == 0 {
        return true;
    }
    let mut tail = [0u8; 8];
    length >= 8
        && file.seek(SeekFrom::Start(length - 8)).is_ok()
        && file.read_exact(&mut tail).is_ok()
        && &tail == TRAILER
}

fn open_prefix(path: &Path, length: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = std::fs::read(path)?;
    bytes.truncate(length);
    Ok(bytes)
}

/// Writes `parts` as the whole store, beside it and renamed over, so that no
/// reader ever sees half of it.
pub fn replace(path: &Path, parts: &[&[u8]]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let aside = aside(path);
    let written = (|| {
        let mut file = options().write(true).create(true).truncate(true).open(&aside)?;
        for part in parts {
            file.write_all(part)?;
        }
        file.sync_data()
    })();
    let result = written.and_then(|()| std::fs::rename(&aside, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&aside);
    }
    result
}

fn aside(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, chunk_count: u32) -> FileRecord<'_> {
        FileRecord {
            path,
            stamp: Stamp { modified: Some((7, 9)), bytes: 42 },
            walked: Kind::Code,
            kind: Kind::Test,
            indexed: true,
            chunk_start: 0,
            chunk_count,
            length: 5.0,
        }
    }

    #[test]
    fn a_segment_reads_back_what_was_written() {
        let mut writer = Writer::new(4);
        let (row, scale) = quantize(&[0.5, -0.5, 0.25, 0.0]);
        writer.file(&record("a.rs", 2), Some((&row, scale)));
        writer.chunk(1, 3, 2.0, false, [("parse", 1u32)].into_iter(), Some((&row, scale)));
        writer.chunk(4, 9, 3.0, true, std::iter::empty(), Some((&row, scale)));
        writer.file(&FileRecord { indexed: false, ..record("b.min.js", 0) }, None);
        writer.term(b"alpha", &[(0, 2), (1, 5)], &[(0, 7)]);
        writer.term(b"beta", &[(1, 1)], &[(0, 1)]);
        let segment = Segment::owned(writer.finish(99)).expect("a segment");

        assert_eq!(segment.id, 99);
        assert_eq!((segment.file_count(), segment.chunk_count()), (2, 2));
        let file = segment.file(0);
        assert_eq!((file.path, file.chunk_start, file.chunk_count, file.kind), ("a.rs", 0, 2, Kind::Test));
        assert_eq!(file.stamp, Stamp { modified: Some((7, 9)), bytes: 42 });
        assert!(!segment.file(1).indexed);
        let chunk = segment.chunk(0);
        assert_eq!((chunk.start_line, chunk.end_line, chunk.imports), (1, 3, false));
        assert_eq!(segment.names(&chunk).collect::<Vec<_>>(), [("parse", 1)]);
        assert!(segment.chunk(1).imports);

        let mut found = Vec::new();
        segment.postings("alpha", |chunk, weight| found.push((chunk, weight)));
        assert_eq!(found, [(0, 2), (1, 5)]);
        found.clear();
        segment.file_postings("beta", |file, weight| found.push((file, weight)));
        assert_eq!(found, [(0, 1)]);
        found.clear();
        segment.postings("gamma", |chunk, weight| found.push((chunk, weight)));
        assert!(found.is_empty());

        let vectors = segment.vectors().expect("inline vectors");
        let asked = [0.5, -0.5, 0.25, 0.0];
        assert!((vectors.chunk_dot(1, &asked) - 0.5625).abs() < 0.01);
        assert!(vectors.file_dot(1, &asked).abs() < f32::EPSILON, "a skipped file has a zero row");
    }

    #[test]
    fn a_store_keeps_whole_records_and_stops_at_a_torn_one() {
        let dir = std::env::temp_dir().join(format!("omega-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("s.idx");
        let segment = |path: &str| {
            let mut writer = Writer::new(0);
            writer.file(&record(path, 0), None);
            writer.finish(new_id())
        };
        let first = segment("a.rs");
        append(&path, &first).unwrap();
        let second = segment("b.rs");
        append(&path, &second[..second.len() - 3]).unwrap();
        let opened = open(&path);
        assert_eq!(opened.segments.len(), 1);
        assert!(!opened.clean());

        // The torn tail is dropped, and what follows can be found.
        drop(opened);
        append(&path, &second).unwrap();
        let opened = open(&path);
        assert!(opened.clean());
        let paths: Vec<&str> = opened.segments.iter().map(|segment| segment.file(0).path).collect();
        assert_eq!(paths, ["a.rs", "b.rs"]);

        // Vectors attached later land on their segment.
        let target = opened.segments[1].id;
        let mut rows = VectorRows::new(2);
        let (row, scale) = quantize(&[1.0, 0.0]);
        rows.file(&row, scale);
        drop(opened);
        append(&path, &rows.attachment(target)).unwrap();
        let opened = open(&path);
        assert!(opened.segments[0].vectors().is_none() && opened.segments[1].vectors().is_some());

        // Overwritten in place, as nothing of omega's ever does, once unmapped.
        drop(opened);
        std::fs::write(&path, b"not a store").unwrap();
        assert!(open(&path).segments.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
