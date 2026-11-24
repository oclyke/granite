//! `verify`: re-hash every file (ignoring hints) and diff against the index.

use std::path::{Path, PathBuf};

use crate::error::{IoContext, Result};
use crate::extsort::{Sorted, Sorter};
use crate::index::IndexReader;
use crate::meta::ScanMeta;
use crate::shard::{GRANITE_DIR, Layout};
use crate::walk::{Item, Walk, relative_if_inside, require_dir};
use crate::{hex, pathenc};

pub struct VerifyOptions {
    pub jobs: usize,
    pub index_dir: Option<PathBuf>,
    pub name: Option<String>,
    pub sort_budget: usize,
}

#[derive(Debug, Clone)]
pub struct Changed {
    pub path: Vec<u8>,
    pub expected: [u8; 32],
    pub actual: [u8; 32],
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub matched: u64,
    /// In the index, absent from disk.
    pub missing: Vec<Vec<u8>>,
    /// On disk, absent from the index.
    pub extra: Vec<Vec<u8>>,
    pub changed: Vec<Changed>,
    pub unreadable: Vec<(Vec<u8>, String)>,
    pub scanned_at: Option<String>,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.changed.is_empty()
    }
}

fn line(path: &[u8], hash: &[u8; 32]) -> Vec<u8> {
    let mut l = pathenc::encode(path);
    l.push(b'\t');
    l.extend_from_slice(hex::encode(hash).as_bytes());
    l
}

/// Splits a sorted `encoded_path \t hex` line. Paths stay encoded so that
/// comparisons agree with the sort order; decode only for reporting.
fn split(line: Vec<u8>, tmp: &Path) -> Result<(Vec<u8>, [u8; 32])> {
    let corrupt = || crate::Error::Malformed {
        path: tmp.to_path_buf(),
        line: 0,
        msg: "corrupt sort spill".into(),
    };
    let at = line.len().checked_sub(65).ok_or_else(corrupt)?;
    let hash = hex::decode_hash(&line[at + 1..]).ok_or_else(corrupt)?;
    let mut enc = line;
    enc.truncate(at);
    Ok((enc, hash))
}

fn decode(enc: &[u8], tmp: &Path) -> Result<Vec<u8>> {
    pathenc::decode(enc).map_err(|_| crate::Error::Malformed {
        path: tmp.to_path_buf(),
        line: 0,
        msg: "corrupt sort spill".into(),
    })
}

pub fn verify(
    root: &Path,
    opts: &VerifyOptions,
    progress: &mut dyn FnMut(u64),
) -> Result<VerifyReport> {
    require_dir(root)?;
    let layout = Layout::resolve(root, opts.index_dir.as_deref(), opts.name.as_deref()).at(root)?;
    let tmp = std::env::temp_dir();

    let mut expected = Sorter::new(&tmp, opts.sort_budget).at(&tmp)?;
    for rec in IndexReader::open(&layout.index)? {
        let rec = rec?;
        expected.push(line(&rec.path, &rec.hash)).at(&tmp)?;
    }

    let mut report = VerifyReport {
        scanned_at: ScanMeta::read(&layout.meta).map(|m| m.started_at),
        ..Default::default()
    };
    let mut exclude = vec![PathBuf::from(GRANITE_DIR)];
    if let Some(inside) = relative_if_inside(root, &layout.dir)? {
        exclude.push(inside);
    }
    let mut actual = Sorter::new(&tmp, opts.sort_budget).at(&tmp)?;
    let none = |_: &[u8], _: u64, _: i128| None;
    let never = |_: &[u8]| false;
    Walk {
        root,
        exclude,
        jobs: opts.jobs,
        precomputed: &none,
        already_done: &never,
    }
    .run(|item| {
        match item {
            Item::File {
                path, hash, size, ..
            } => {
                progress(size);
                actual.push(line(&path, &hash)).at(&tmp)?
            }
            Item::Skipped { path, reason } => {
                let benign = reason == "empty directory"
                    || reason == "symlink"
                    || reason == "not a regular file";
                if !benign {
                    report.unreadable.push((path, reason));
                }
            }
        }
        Ok(())
    })?;

    // Merge-join two path-sorted streams.
    let mut exp = expected.finish().at(&tmp)?;
    let mut act = actual.finish().at(&tmp)?;
    let pull = |it: &mut Sorted| -> Result<Option<(Vec<u8>, [u8; 32])>> {
        it.next().map(|l| split(l.at(&tmp)?, &tmp)).transpose()
    };
    let mut e = pull(&mut exp)?;
    let mut a = pull(&mut act)?;
    loop {
        match (e.take(), a.take()) {
            (None, None) => break,
            (Some((p, _)), None) => {
                report.missing.push(decode(&p, &tmp)?);
                e = pull(&mut exp)?;
            }
            (None, Some((p, _))) => {
                report.extra.push(decode(&p, &tmp)?);
                a = pull(&mut act)?;
            }
            (Some(ex), Some(ac)) => match ex.0.cmp(&ac.0) {
                std::cmp::Ordering::Less => {
                    report.missing.push(decode(&ex.0, &tmp)?);
                    a = Some(ac);
                    e = pull(&mut exp)?;
                }
                std::cmp::Ordering::Greater => {
                    report.extra.push(decode(&ac.0, &tmp)?);
                    e = Some(ex);
                    a = pull(&mut act)?;
                }
                std::cmp::Ordering::Equal => {
                    if ex.1 == ac.1 {
                        report.matched += 1;
                    } else {
                        report.changed.push(Changed {
                            path: decode(&ex.0, &tmp)?,
                            expected: ex.1,
                            actual: ac.1,
                        });
                    }
                    e = pull(&mut exp)?;
                    a = pull(&mut act)?;
                }
            },
        }
    }
    Ok(report)
}
