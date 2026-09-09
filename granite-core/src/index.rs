//! The canonical index: a header (§4.2) followed by sorted `hash\tsize\tpath`
//! records (§4.3). This file is a pure function of a shard's content and
//! relative paths; nothing volatile may appear here.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::header::Header;
use crate::{hex, pathenc};

pub const FORMAT_VERSION: u64 = 0;
pub const PRODUCER: &str = concat!("granite ", env!("CARGO_PKG_VERSION"));
pub const HASH: &str = "blake3-256";
pub const PATH_ENCODING: &str = "escaped-bytes-v1";
pub const SORT: &str = "line-bytewise";
pub const RECORD_FIELDS: [&str; 3] = ["hash", "size", "path"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexHeader {
    pub version: u64,
    pub producer: String,
    pub hash: String,
    pub path_encoding: String,
    pub record_fields: Vec<String>,
    pub sort: String,
    pub requires: Vec<String>,
}

impl IndexHeader {
    pub fn current() -> Self {
        IndexHeader {
            version: FORMAT_VERSION,
            producer: PRODUCER.to_owned(),
            hash: HASH.to_owned(),
            path_encoding: PATH_ENCODING.to_owned(),
            record_fields: RECORD_FIELDS.iter().map(|s| (*s).to_owned()).collect(),
            sort: SORT.to_owned(),
            requires: Vec::new(),
        }
    }

    pub fn to_header(&self) -> Header {
        let mut h = Header::default();
        h.push("granite_index_version", self.version.to_string());
        h.push("producer", self.producer.clone());
        h.push("hash", self.hash.clone());
        h.push("path_encoding", self.path_encoding.clone());
        h.push_seq("record_fields", &self.record_fields);
        h.push("sort", self.sort.clone());
        h.push_seq("requires", &self.requires);
        h
    }

    pub fn from_header(h: &Header, path: &Path) -> Result<Self> {
        let malformed = |msg: String| Error::Malformed {
            path: path.to_path_buf(),
            line: 0,
            msg,
        };
        let scalar = |key: &str| {
            h.scalar(key)
                .map(str::to_owned)
                .ok_or_else(|| malformed(format!("header is missing `{key}`")))
        };
        let seq = |key: &str| {
            h.seq(key)
                .map(<[String]>::to_vec)
                .ok_or_else(|| malformed(format!("header is missing sequence `{key}`")))
        };
        let version = scalar("granite_index_version")?
            .parse()
            .map_err(|_| malformed("`granite_index_version` is not an integer".into()))?;
        Ok(IndexHeader {
            version,
            producer: scalar("producer")?,
            hash: scalar("hash")?,
            path_encoding: scalar("path_encoding")?,
            record_fields: seq("record_fields")?,
            sort: scalar("sort")?,
            requires: seq("requires")?,
        })
    }

    /// The version gate (§5) and semantic-key checks (§5.4).
    pub fn check(&self, path: &Path) -> Result<()> {
        let refuse = |msg: String| Error::Refused {
            msg: format!("{msg}\n       (in {})", path.display()),
        };
        match self.version.cmp(&FORMAT_VERSION) {
            std::cmp::Ordering::Greater => {
                return Err(refuse(format!(
                    "index format version {} is newer than this {PRODUCER} understands (version {FORMAT_VERSION})\nhint:  upgrade granite",
                    self.version
                )));
            }
            // No older format has a documented reader yet. When one does, dispatch
            // to it here; never upgrade the file in place.
            std::cmp::Ordering::Less => {
                return Err(refuse(format!(
                    "index format version {} has no reader in {PRODUCER}\nhint:  a future `granite migrate` will convert it",
                    self.version
                )));
            }
            std::cmp::Ordering::Equal => {}
        }
        // Alpha rule (§5.1): while the format version is 0, only indexes written by
        // this exact build are accepted. Delete this block, and nothing else, when
        // FORMAT_VERSION becomes 1.
        if self.version == 0 && self.producer != PRODUCER {
            return Err(refuse(format!(
                "index was written by {}, this is {PRODUCER}\n       index format version 0 is unstable; formats from different alpha builds\n       are not interchangeable\nhint:  re-run `granite scan --force` on this shard",
                self.producer
            )));
        }
        if let Some(unknown) = self
            .requires
            .iter()
            .find(|r| !KNOWN_EXTENSIONS.contains(&r.as_str()))
        {
            return Err(refuse(format!(
                "index requires extension `{unknown}`, which {PRODUCER} does not understand\nhint:  upgrade granite"
            )));
        }
        if self.hash != HASH {
            return Err(refuse(format!(
                "index uses hash `{}`; this granite only understands `{HASH}`",
                self.hash
            )));
        }
        if self.path_encoding != PATH_ENCODING {
            return Err(refuse(format!(
                "index uses path encoding `{}`; this granite only understands `{PATH_ENCODING}`",
                self.path_encoding
            )));
        }
        if self.record_fields != RECORD_FIELDS {
            return Err(refuse(format!(
                "index record fields are [{}]; this granite expects [{}]",
                self.record_fields.join(", "),
                RECORD_FIELDS.join(", ")
            )));
        }
        if self.sort != SORT {
            return Err(refuse(format!(
                "index sort order is `{}`; this granite expects `{SORT}`",
                self.sort
            )));
        }
        Ok(())
    }
}

/// Names this build accepts in `requires`. v0 defines none.
const KNOWN_EXTENSIONS: [&str; 0] = [];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub hash: [u8; 32],
    pub size: u64,
    /// Raw path bytes, relative to the shard root.
    pub path: Vec<u8>,
}

impl Record {
    pub fn hex(&self) -> String {
        hex::encode(&self.hash)
    }

    /// The record's line, without the trailing newline. Sort keys are these bytes.
    pub fn line(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + 24 + self.path.len());
        line_into(&self.hash, self.size, &self.path, &mut out);
        out
    }

    pub fn parse_line(line: &[u8]) -> std::result::Result<Record, String> {
        let mut fields = line.split(|&b| b == b'\t');
        let (Some(h), Some(s), Some(p), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err("record does not have exactly three tab-separated fields".into());
        };
        let hash = hex::decode_hash(h).ok_or("hash is not 64 lowercase hex characters")?;
        let size = parse_size(s)?;
        let path = pathenc::decode(p).map_err(|e| e.to_string())?;
        pathenc::validate_relative(&path)?;
        Ok(Record { hash, size, path })
    }
}

pub fn line_into(hash: &[u8; 32], size: u64, path: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(hex::encode(hash).as_bytes());
    out.push(b'\t');
    out.extend_from_slice(size.to_string().as_bytes());
    out.push(b'\t');
    pathenc::encode_into(path, out);
}

pub fn parse_size(s: &[u8]) -> std::result::Result<u64, String> {
    let ok = !s.is_empty() && s.iter().all(u8::is_ascii_digit) && (s == b"0" || s[0] != b'0');
    if !ok {
        return Err("size is not a canonical decimal integer".into());
    }
    std::str::from_utf8(s)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "size does not fit in 64 bits".into())
}

/// Streams records from an index, enforcing the header gate and the sort and
/// uniqueness invariants as it goes.
pub struct IndexReader<R> {
    r: R,
    path: PathBuf,
    pub header: IndexHeader,
    prev: Vec<u8>,
    line_no: u64,
}

impl IndexReader<BufReader<File>> {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).at(path)?;
        Self::new(BufReader::with_capacity(1 << 16, file), path)
    }
}

impl<R: BufRead> IndexReader<R> {
    pub fn new(mut r: R, path: &Path) -> Result<Self> {
        let raw = Header::parse(&mut r).at(path)?;
        let header = IndexHeader::from_header(&raw, path)?;
        header.check(path)?;
        Ok(IndexReader {
            r,
            path: path.to_path_buf(),
            header,
            prev: Vec::new(),
            line_no: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn malformed(&self, msg: impl Into<String>) -> Error {
        Error::Malformed {
            path: self.path.clone(),
            line: self.line_no,
            msg: msg.into(),
        }
    }

    fn read_record(&mut self) -> Result<Option<Record>> {
        let mut line = Vec::new();
        if self.r.read_until(b'\n', &mut line).at(&self.path)? == 0 {
            return Ok(None);
        }
        self.line_no += 1;
        if line.pop() != Some(b'\n') {
            return Err(self.malformed("missing final newline"));
        }
        if self.line_no > 1 && line <= self.prev {
            return Err(self.malformed("records are not in strictly ascending bytewise order"));
        }
        let rec = Record::parse_line(&line).map_err(|e| self.malformed(e))?;
        self.prev = line;
        Ok(Some(rec))
    }
}

impl<R: BufRead> Iterator for IndexReader<R> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        self.read_record().transpose()
    }
}

/// Writes an index, hashing the bytes as they go so the fingerprint is known
/// without a second read.
pub struct IndexWriter<W: Write> {
    w: W,
    hasher: blake3::Hasher,
    prev: Option<Vec<u8>>,
    pub records: u64,
}

impl<W: Write> IndexWriter<W> {
    pub fn new(mut w: W) -> io::Result<Self> {
        let head = IndexHeader::current().to_header().emit();
        w.write_all(&head)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&head);
        Ok(IndexWriter {
            w,
            hasher,
            prev: None,
            records: 0,
        })
    }

    /// `line` is a record without its newline; lines must arrive strictly ascending.
    pub fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        if self.prev.as_deref().is_some_and(|p| line <= p) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "index records written out of order",
            ));
        }
        self.w.write_all(line)?;
        self.w.write_all(b"\n")?;
        self.hasher.update(line);
        self.hasher.update(b"\n");
        self.prev = Some(line.to_vec());
        self.records += 1;
        Ok(())
    }

    pub fn write(&mut self, rec: &Record) -> io::Result<()> {
        self.write_line(&rec.line())
    }

    pub fn finish(mut self) -> io::Result<[u8; 32]> {
        self.w.flush()?;
        Ok(*self.hasher.finalize().as_bytes())
    }
}

/// BLAKE3 of the whole file, header included (§4.5).
pub fn fingerprint(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).at(path)?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher).at(path)?;
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(bytes: &[u8]) -> Result<Vec<Record>> {
        IndexReader::new(io::Cursor::new(bytes), Path::new("test.index"))?.collect()
    }

    fn with_header(records: &str) -> Vec<u8> {
        let mut v = IndexHeader::current().to_header().emit();
        v.extend_from_slice(records.as_bytes());
        v
    }

    const H0: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn parses_and_enforces_order() {
        let recs = read_all(&with_header(&format!(
            "{H0}\t0\ta\n{H0}\t0\tb\\n\n{H1}\t7\tc\n"
        )))
        .unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[1].path, b"b\n");
        assert!(read_all(&with_header(&format!("{H1}\t7\tc\n{H0}\t0\ta\n"))).is_err());
        assert!(read_all(&with_header(&format!("{H0}\t0\ta\n{H0}\t0\ta\n"))).is_err());
        assert!(read_all(&with_header(&format!("{H0}\t0\ta"))).is_err());
        assert!(read_all(&with_header(&format!("{H0}\t01\ta\n"))).is_err());
        assert!(read_all(&with_header(&format!("{}\t1\ta\n", "A".repeat(64)))).is_err());
        assert!(read_all(&with_header(&format!("{H0}\t1\t../a\n"))).is_err());
    }

    #[test]
    fn version_gate() {
        let base = IndexHeader::current();
        let refused =
            |h: IndexHeader| matches!(h.check(Path::new("x")), Err(Error::Refused { .. }));
        assert!(base.check(Path::new("x")).is_ok());
        assert!(refused(IndexHeader {
            version: 1,
            ..base.clone()
        }));
        assert!(refused(IndexHeader {
            producer: "granite 9.9.9".into(),
            ..base.clone()
        }));
        assert!(refused(IndexHeader {
            requires: vec!["tombstones".into()],
            ..base.clone()
        }));
        assert!(refused(IndexHeader {
            hash: "sha256".into(),
            ..base.clone()
        }));
        assert!(refused(IndexHeader {
            path_encoding: "utf8".into(),
            ..base.clone()
        }));
        assert!(refused(IndexHeader {
            record_fields: vec!["hash".into(), "path".into()],
            ..base.clone()
        }));
        assert!(refused(IndexHeader {
            sort: "none".into(),
            ..base
        }));
    }

    #[test]
    fn unknown_header_keys_are_ignored() {
        let mut h = IndexHeader::current().to_header();
        h.push("future_key", "whatever");
        let mut bytes = h.emit();
        bytes.extend_from_slice(format!("{H0}\t0\ta\n").as_bytes());
        assert_eq!(read_all(&bytes).unwrap().len(), 1);
    }
}
