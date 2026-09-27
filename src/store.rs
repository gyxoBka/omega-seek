use crate::index::Kind;
use memmap2::Mmap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const MAGIC: &[u8; 8] = b"OMEGAIDX";
const TRAILER: &[u8; 8] = b"XDIAGEMO";
const HEADER: usize = 24;
const FOOTER: usize = 16;
pub const VERSION: u32 = 17;

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
const PREFIX: usize = 8 + SECTIONS * 16;

const FILE_RECORD: usize = 60;
const CHUNK_RECORD: usize = 24;
const NAME_RECORD: usize = 12;
const TERM_RECORD: usize = 32;
const VECTORS_PREFIX: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stamp {
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

#[derive(Clone, Copy, Debug)]
pub struct FileRecord<'a> {
    pub path: &'a str,
    pub stamp: Stamp,
    pub walked: Kind,
    pub kind: Kind,
    pub indexed: bool,
    pub chunk_start: u32,
    pub chunk_count: u32,
    pub length: f32,
    pub hash: [u8; 16],
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

pub struct Segment {
    bytes: Bytes,
    pub id: u64,
    sections: [(usize, usize); SECTIONS],
    files: usize,
    chunks: usize,
    terms: usize,
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
    pub fn owned(bytes: Vec<u8>) -> Option<Arc<Self>> {
        let length = bytes.len();
        let bytes = Bytes::Owned(Arc::new(bytes));
        let (kind, payload, end) = frame(&bytes, 0)?;
        (kind == LEXICAL && end == length).then(|| Self::parse(bytes.clone(), payload, end - FOOTER - payload))?
    }

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
            hash: bytes[at + 44..at + 60].try_into().unwrap_or_default(),
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

    pub fn postings(&self, term: &str, mut each: impl FnMut(u32, u32)) {
        if let Some(found) = self.find(term) {
            let record = self.term_record(found);
            self.decode(POSTINGS, record.postings, record.postings_count, &mut each);
        }
    }

    pub fn file_postings(&self, term: &str, mut each: impl FnMut(u32, u32)) {
        if let Some(found) = self.find(term) {
            let record = self.term_record(found);
            self.decode(FILE_POSTINGS, record.file_postings, record.file_postings_count, &mut each);
        }
    }

    pub fn postings_at(&self, index: usize, mut each: impl FnMut(u32, u32)) {
        let record = self.term_record(index);
        self.decode(POSTINGS, record.postings, record.postings_count, &mut each);
    }

    pub fn file_postings_at(&self, index: usize, mut each: impl FnMut(u32, u32)) {
        let record = self.term_record(index);
        self.decode(FILE_POSTINGS, record.file_postings, record.file_postings_count, &mut each);
    }

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

    pub fn attach(&self, vectors: Vectors) -> bool {
        vectors.chunks == self.chunks && vectors.files == self.files && self.vectors.set(vectors).is_ok()
    }
}

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

    #[must_use]
    pub fn attachment(&self, target: u64) -> Vec<u8> {
        let mut payload = target.to_le_bytes().to_vec();
        self.encode(&mut payload);
        framed(VECTORS, &payload)
    }

    #[must_use]
    pub fn into_vectors(self) -> Option<Vectors> {
        let mut bytes = Vec::new();
        self.encode(&mut bytes);
        let length = bytes.len();
        Vectors::parse(Bytes::Owned(Arc::new(bytes)), 0, length)
    }
}

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
    open_file: Option<usize>,
}

impl Writer {
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
        self.files.extend_from_slice(&record.hash);
        if let Some(rows) = &mut self.vectors {
            let (row, scale) = vector.unwrap_or((&[], 0.0));
            rows.file(row, scale);
        }
    }

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

#[must_use]
pub fn new_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    let mixed = nanos ^ (u64::from(std::process::id()) << 40) ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(20);
    let mut z = mixed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[derive(Debug, Default)]
pub struct Opened {
    pub segments: Vec<Arc<Segment>>,
    pub clean_end: usize,
    pub length: usize,
}

impl Opened {
    #[must_use]
    pub fn clean(&self) -> bool {
        self.clean_end == self.length
    }
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0x1 | 0x2 | 0x4);
    }
    options
}

#[allow(unsafe_code)]
fn map(file: &File) -> std::io::Result<Mmap> {
    unsafe { Mmap::map(file) }
}

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
    file.write_all(record)
}

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

pub fn touch(path: &Path) {
    if let Ok(file) = options().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

pub const ABANDONED: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);
const LEFT_ASIDE: std::time::Duration = std::time::Duration::from_secs(3600);

#[must_use]
pub fn prune(dir: &Path, abandoned: std::time::Duration) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return (0, 0) };
    let now = std::time::SystemTime::now();
    let mut removed = (0, 0);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let limit = if name.ends_with(".idx") || name.ends_with(".bin") {
            abandoned
        } else if name.ends_with(".tmp") {
            LEFT_ASIDE
        } else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        let age = meta.modified().ok().and_then(|at| now.duration_since(at).ok()).unwrap_or_default();
        if meta.is_file() && age > limit && std::fs::remove_file(entry.path()).is_ok() {
            removed.0 += 1;
            removed.1 += meta.len();
        }
    }
    removed
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
            hash: [7; 16],
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
        assert_eq!(file.hash, [7; 16]);
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

        drop(opened);
        append(&path, &second).unwrap();
        let opened = open(&path);
        assert!(opened.clean());
        let paths: Vec<&str> = opened.segments.iter().map(|segment| segment.file(0).path).collect();
        assert_eq!(paths, ["a.rs", "b.rs"]);

        let target = opened.segments[1].id;
        let mut rows = VectorRows::new(2);
        let (row, scale) = quantize(&[1.0, 0.0]);
        rows.file(&row, scale);
        drop(opened);
        append(&path, &rows.attachment(target)).unwrap();
        let opened = open(&path);
        assert!(opened.segments[0].vectors().is_none() && opened.segments[1].vectors().is_some());

        drop(opened);
        std::fs::write(&path, b"not a store").unwrap();
        assert!(open(&path).segments.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
