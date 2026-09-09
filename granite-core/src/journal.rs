//! `scan.journal`: append-only log of hashed files, unsorted. Its presence marks
//! an incomplete scan; the canonical index and hints are derived from it.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::header::Header;
use crate::index::{PRODUCER, parse_size};
use crate::{hex, pathenc};

const JOURNAL_VERSION: u64 = 0;

/// Fsync cadence: bounded loss on crash without an fsync per file, which would
/// dominate a scan of many small files.
const FSYNC_EVERY_RECORDS: u64 = 1000;
const FSYNC_EVERY: Duration = Duration::from_secs(5);

pub(crate) struct Entry {
    pub hash: [u8; 32],
    pub size: u64,
    pub mtime_nanos: i128,
    pub path: Vec<u8>,
}

fn header() -> Header {
    let mut h = Header::default();
    h.push("granite_journal_version", JOURNAL_VERSION.to_string());
    h.push("producer", PRODUCER);
    h
}

fn parse_line(line: &[u8]) -> Option<Entry> {
    let mut f = line.split(|&b| b == b'\t');
    let (h, s, m, p) = (f.next()?, f.next()?, f.next()?, f.next()?);
    if f.next().is_some() {
        return None;
    }
    Some(Entry {
        hash: hex::decode_hash(h)?,
        size: parse_size(s).ok()?,
        mtime_nanos: std::str::from_utf8(m).ok()?.parse().ok()?,
        path: pathenc::decode(p).ok()?,
    })
}

pub(crate) enum Loaded {
    /// Written by this build: resume, skipping `done`. `valid_len` is the file
    /// length through the last complete record; anything after it is a torn write.
    Resume {
        done: HashSet<Vec<u8>>,
        valid_len: u64,
    },
    /// Written by another build: discard and start over.
    Foreign { producer: String },
}

/// Loads a journal left by an interrupted scan. A trailing partial line (the
/// crash point) is ignored; any other malformed line is an error.
pub(crate) fn load(path: &Path) -> io::Result<Loaded> {
    let mut r = BufReader::with_capacity(1 << 16, File::open(path)?);
    let h = Header::parse(&mut r)?;
    let producer = h.scalar("producer").unwrap_or("unknown").to_owned();
    if h.scalar("granite_journal_version") != Some(&JOURNAL_VERSION.to_string())
        || producer != PRODUCER
    {
        return Ok(Loaded::Foreign { producer });
    }
    let mut done = HashSet::new();
    let mut valid_len = r.stream_position()?;
    let mut line = Vec::new();
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.pop() != Some(b'\n') {
            break;
        }
        let entry = parse_line(&line).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "malformed journal record")
        })?;
        done.insert(entry.path);
        valid_len += line.len() as u64 + 1;
    }
    Ok(Loaded::Resume { done, valid_len })
}

/// Streams every complete record of a journal.
pub(crate) fn read(path: &Path) -> io::Result<impl Iterator<Item = io::Result<Entry>>> {
    let mut r = BufReader::with_capacity(1 << 16, File::open(path)?);
    Header::parse(&mut r)?;
    Ok(std::iter::from_fn(move || {
        let mut line = Vec::new();
        match r.read_until(b'\n', &mut line) {
            Err(e) => Some(Err(e)),
            Ok(0) => None,
            Ok(_) => {
                if line.pop() != Some(b'\n') {
                    return None;
                }
                Some(parse_line(&line).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "malformed journal record")
                }))
            }
        }
    }))
}

pub(crate) struct Writer {
    w: BufWriter<File>,
    since_sync: u64,
    last_sync: Instant,
}

impl Writer {
    /// Creates the journal, or resumes one truncated to its last complete record.
    pub(crate) fn open(path: &Path, resume_at: Option<u64>) -> io::Result<Writer> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        match resume_at {
            Some(len) => file.set_len(len)?,
            None => {
                file.set_len(0)?;
                file.write_all(&header().emit())?;
            }
        }
        Ok(Writer {
            w: BufWriter::with_capacity(1 << 16, file),
            since_sync: 0,
            last_sync: Instant::now(),
        })
    }

    pub(crate) fn append(&mut self, e: &Entry) -> io::Result<()> {
        write!(
            self.w,
            "{}\t{}\t{}\t",
            hex::encode(&e.hash),
            e.size,
            e.mtime_nanos
        )?;
        self.w.write_all(&pathenc::encode(&e.path))?;
        self.w.write_all(b"\n")?;
        self.since_sync += 1;
        if self.since_sync >= FSYNC_EVERY_RECORDS || self.last_sync.elapsed() >= FSYNC_EVERY {
            self.sync()?;
        }
        Ok(())
    }

    pub(crate) fn sync(&mut self) -> io::Result<()> {
        self.w.flush()?;
        self.w.get_ref().sync_data()?;
        self.since_sync = 0;
        self.last_sync = Instant::now();
        Ok(())
    }
}
