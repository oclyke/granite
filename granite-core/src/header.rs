//! The YAML header shared by every granite file: a flat mapping of scalars and
//! flat sequences of scalars, delimited by `---` lines.
//!
//! Headers are emitted by hand (fixed key order, `key: value` per line) so the
//! bytes are stable across YAML library versions; they are parsed with a real
//! YAML parser so that any conforming writer is accepted.

use std::io::{self, BufRead};

use yaml_rust2::{Yaml, YamlLoader};

/// Bound on the header region, so a non-granite file fails fast.
const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Scalar(String),
    Seq(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    pub fields: Vec<(String, Value)>,
}

impl Header {
    pub fn push(&mut self, key: &str, value: impl Into<String>) {
        self.fields
            .push((key.to_owned(), Value::Scalar(value.into())));
    }

    pub fn push_seq(&mut self, key: &str, values: &[impl AsRef<str>]) {
        let seq = values.iter().map(|v| v.as_ref().to_owned()).collect();
        self.fields.push((key.to_owned(), Value::Seq(seq)));
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn scalar(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Scalar(s) => Some(s),
            Value::Seq(_) => None,
        }
    }

    pub fn seq(&self, key: &str) -> Option<&[String]> {
        match self.get(key)? {
            Value::Seq(s) => Some(s),
            Value::Scalar(_) => None,
        }
    }

    /// The mapping lines alone, without delimiters (the `scan.meta` form).
    pub fn emit_body(&self, out: &mut Vec<u8>) {
        for (k, v) in &self.fields {
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(b": ");
            match v {
                Value::Scalar(s) => out.extend_from_slice(s.as_bytes()),
                Value::Seq(items) => {
                    out.push(b'[');
                    out.extend_from_slice(items.join(", ").as_bytes());
                    out.push(b']');
                }
            }
            out.push(b'\n');
        }
    }

    /// `---`, the mapping, `---`: the form that precedes TSV records.
    pub fn emit(&self) -> Vec<u8> {
        let mut out = b"---\n".to_vec();
        self.emit_body(&mut out);
        out.extend_from_slice(b"---\n");
        out
    }

    pub fn parse_body(body: &str) -> io::Result<Header> {
        let docs = YamlLoader::load_from_str(body).map_err(invalid)?;
        let doc = match docs.as_slice() {
            [doc] => doc,
            [] => return Err(invalid("header is empty")),
            _ => return Err(invalid("header contains more than one YAML document")),
        };
        let Yaml::Hash(map) = doc else {
            return Err(invalid("header is not a mapping"));
        };
        let mut fields = Vec::with_capacity(map.len());
        for (k, v) in map {
            let Yaml::String(key) = k else {
                return Err(invalid("header key is not a string"));
            };
            let value = match v {
                Yaml::Array(items) => {
                    let mut seq = Vec::with_capacity(items.len());
                    for item in items {
                        seq.push(scalar_string(item)?);
                    }
                    Value::Seq(seq)
                }
                other => Value::Scalar(scalar_string(other)?),
            };
            fields.push((key.clone(), value));
        }
        Ok(Header { fields })
    }

    /// Consumes the delimited header from `r`, leaving it positioned at the
    /// first record.
    pub fn parse(r: &mut impl BufRead) -> io::Result<Header> {
        let mut line = Vec::new();
        r.read_until(b'\n', &mut line)?;
        if line != b"---\n" {
            return Err(invalid("file does not start with a `---` header line"));
        }
        let mut body = Vec::new();
        loop {
            line.clear();
            if r.read_until(b'\n', &mut line)? == 0 {
                return Err(invalid("header is not terminated by a `---` line"));
            }
            if line == b"---\n" {
                break;
            }
            body.extend_from_slice(&line);
            if body.len() > MAX_HEADER_BYTES {
                return Err(invalid("header exceeds the maximum size"));
            }
        }
        let body = std::str::from_utf8(&body).map_err(|_| invalid("header is not UTF-8"))?;
        Self::parse_body(body)
    }
}

fn scalar_string(v: &Yaml) -> io::Result<String> {
    match v {
        Yaml::String(s) | Yaml::Real(s) => Ok(s.clone()),
        Yaml::Integer(i) => Ok(i.to_string()),
        Yaml::Boolean(b) => Ok(b.to_string()),
        _ => Err(invalid("header value is not a scalar or flat sequence")),
    }
}

fn invalid(msg: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_byte_for_byte() {
        let text = b"---\ngranite_index_version: 0\nproducer: granite 0.1.0\nhash: blake3-256\npath_encoding: escaped-bytes-v1\nrecord_fields: [hash, size, path]\nsort: line-bytewise\nrequires: []\n---\nrecord\n";
        let mut cursor = io::Cursor::new(&text[..]);
        let h = Header::parse(&mut cursor).unwrap();
        assert_eq!(h.scalar("granite_index_version"), Some("0"));
        assert_eq!(h.seq("requires"), Some(&[][..]));
        assert_eq!(h.seq("record_fields").unwrap().len(), 3);
        let emitted = h.emit();
        assert_eq!(&text[..emitted.len()], &emitted[..]);
        assert_eq!(cursor.position() as usize, emitted.len());
    }

    #[test]
    fn rejects_non_flat_headers() {
        assert!(Header::parse_body("a:\n  b: 1\n").is_err());
        assert!(Header::parse_body("- a\n").is_err());
        let mut c = io::Cursor::new(&b"no header\n"[..]);
        assert!(Header::parse(&mut c).is_err());
        let mut c = io::Cursor::new(&b"---\na: 1\n"[..]);
        assert!(Header::parse(&mut c).is_err());
    }
}
