//! `.granite/hints`: the rescan-skip cache. Only `scan` may touch this module;
//! every other command must derive its view of a shard from `index` or from the
//! bytes on disk, never from a cache that could be stale. Keep this module
//! private to the crate and imported only from `scan`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;

use crate::fsutil::write_atomic;
use crate::header::Header;
use crate::index::{HASH, PATH_ENCODING, PRODUCER, SORT, parse_size};
use crate::{hex, pathenc};

pub(crate) const HINTS_VERSION: u64 = 0;

pub(crate) struct Hints {
    map: HashMap<Vec<u8>, ([u8; 32], u64, i128)>,
}

impl Hints {
    /// Any problem yields an error the caller logs and ignores: hints are never load-bearing.
    pub(crate) fn load(path: &Path) -> io::Result<Hints> {
        let mut r = BufReader::with_capacity(1 << 16, File::open(path)?);
        let h = Header::parse(&mut r)?;
        let ok = h.scalar("granite_hints_version") == Some(&HINTS_VERSION.to_string())
            && h.scalar("producer") == Some(PRODUCER)
            && h.scalar("hash") == Some(HASH)
            && h.scalar("path_encoding") == Some(PATH_ENCODING);
        if !ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "hints were written by a different granite build",
            ));
        }
        let mut map = HashMap::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if io::BufRead::read_until(&mut r, b'\n', &mut line)? == 0 {
                break;
            }
            line.pop();
            let entry = parse_line(&line).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed hint record")
            })?;
            map.insert(entry.0, (entry.3, entry.1, entry.2));
        }
        Ok(Hints { map })
    }

    pub(crate) fn lookup(&self, path: &[u8], size: u64, mtime_nanos: i128) -> Option<[u8; 32]> {
        match self.map.get(path) {
            Some(&(hash, s, m)) if s == size && m == mtime_nanos => Some(hash),
            _ => None,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

fn parse_line(line: &[u8]) -> Option<(Vec<u8>, u64, i128, [u8; 32])> {
    let mut f = line.split(|&b| b == b'\t');
    let (p, s, m, h) = (f.next()?, f.next()?, f.next()?, f.next()?);
    if f.next().is_some() {
        return None;
    }
    let path = pathenc::decode(p).ok()?;
    let size = parse_size(s).ok()?;
    let mtime = std::str::from_utf8(m).ok()?.parse().ok()?;
    Some((path, size, mtime, hex::decode_hash(h)?))
}

pub(crate) fn line(path: &[u8], size: u64, mtime_nanos: i128, hash: &[u8; 32]) -> Vec<u8> {
    let mut out = pathenc::encode(path);
    out.extend_from_slice(format!("\t{size}\t{mtime_nanos}\t{}", hex::encode(hash)).as_bytes());
    out
}

pub(crate) fn write(
    path: &Path,
    lines: impl Iterator<Item = io::Result<Vec<u8>>>,
) -> io::Result<()> {
    let mut h = Header::default();
    h.push("granite_hints_version", HINTS_VERSION.to_string());
    h.push("producer", PRODUCER);
    h.push("hash", HASH);
    h.push("path_encoding", PATH_ENCODING);
    h.push_seq("record_fields", &["path", "size", "mtime_nanos", "hash"]);
    h.push("sort", SORT);
    write_atomic(path, |w| {
        w.write_all(&h.emit())?;
        for line in lines {
            w.write_all(&line?)?;
            w.write_all(b"\n")?;
        }
        Ok(())
    })
}
