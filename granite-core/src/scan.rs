use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{IoContext, Result};
use crate::extsort::Sorter;
use crate::fsutil::{fsync_dir, write_atomic};
use crate::header::Header;
use crate::hints::{self, Hints};
use crate::index::{IndexWriter, line_into};
use crate::journal::{self, Loaded};
use crate::meta::{ScanMeta, rfc3339_now};
use crate::shard::{GRANITE_DIR, Layout};
use crate::walk::{Item, Walk, relative_if_inside, require_dir};
use crate::{hex, pathenc};

pub struct ScanOptions {
    pub index_dir: Option<PathBuf>,
    pub name: Option<String>,
    pub force: bool,
    pub jobs: usize,
    pub sort_budget: usize,
}

pub struct ScanReport {
    pub files_indexed: u64,
    pub bytes_indexed: u64,
    pub files_skipped: u64,
    pub fingerprint: [u8; 32],
    pub resumed: bool,
    pub layout: Layout,
}

pub fn scan(
    root: &Path,
    opts: &ScanOptions,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(u64),
) -> Result<ScanReport> {
    require_dir(root)?;
    let started_at = rfc3339_now();
    let layout = Layout::resolve(root, opts.index_dir.as_deref(), opts.name.as_deref()).at(root)?;
    fs::create_dir_all(&layout.dir).at(&layout.dir)?;

    let mut done = HashSet::new();
    let mut resume_at = None;
    if layout.journal.exists() {
        match journal::load(&layout.journal).at(&layout.journal)? {
            Loaded::Resume { done: d, valid_len } => {
                log(&format!(
                    "resuming interrupted scan: {} files already hashed",
                    d.len()
                ));
                done = d;
                resume_at = Some(valid_len);
            }
            Loaded::Foreign { producer } => {
                log(&format!(
                    "discarding journal left by {producer}; starting a full scan"
                ));
            }
        }
    }

    let hints = if opts.force {
        None
    } else {
        match Hints::load(&layout.hints) {
            Ok(h) => {
                log(&format!("using {} hints from a previous scan", h.len()));
                Some(h)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                log(&format!("ignoring hints ({e}); reading every file"));
                None
            }
        }
    };

    let mut exclude = vec![PathBuf::from(GRANITE_DIR)];
    if let Some(inside) = relative_if_inside(root, &layout.dir)? {
        exclude.push(inside);
    }

    let mut jw = journal::Writer::open(&layout.journal, resume_at).at(&layout.journal)?;
    let skipped_tmp =
        tempfile::NamedTempFile::with_prefix_in(".granite-tmp.", &layout.dir).at(&layout.dir)?;
    let mut skipped_w = std::io::BufWriter::new(skipped_tmp.as_file());
    let mut skipped_h = Header::default();
    skipped_h.push("granite_skipped_version", "0");
    skipped_h.push("path_encoding", crate::index::PATH_ENCODING);
    skipped_h.push_seq("record_fields", &["path", "reason"]);
    skipped_w.write_all(&skipped_h.emit()).at(&layout.skipped)?;
    let mut files_skipped = 0u64;

    let precomputed = |p: &[u8], s: u64, m: i128| hints.as_ref().and_then(|h| h.lookup(p, s, m));
    let already_done = |p: &[u8]| done.contains(p);
    Walk {
        root,
        exclude,
        jobs: opts.jobs,
        precomputed: &precomputed,
        already_done: &already_done,
    }
    .run(|item| {
        match item {
            Item::File {
                path,
                size,
                mtime_nanos,
                hash,
            } => {
                progress(size);
                jw.append(&journal::Entry {
                    hash,
                    size,
                    mtime_nanos,
                    path,
                })
                .at(&layout.journal)?;
            }
            Item::Skipped { path, reason } => {
                files_skipped += 1;
                skipped_w
                    .write_all(&pathenc::encode(&path))
                    .at(&layout.skipped)?;
                writeln!(skipped_w, "\t{reason}").at(&layout.skipped)?;
            }
        }
        Ok(())
    })?;
    jw.sync().at(&layout.journal)?;
    drop(jw);

    let (fingerprint, files_indexed, bytes_indexed) = write_index(&layout, opts.sort_budget)?;
    write_hints(&layout, opts.sort_budget)?;

    skipped_w.flush().at(&layout.skipped)?;
    drop(skipped_w);
    skipped_tmp.as_file().sync_all().at(&layout.skipped)?;
    skipped_tmp
        .persist(&layout.skipped)
        .map_err(|e| e.error)
        .at(&layout.skipped)?;

    ScanMeta {
        started_at,
        finished_at: rfc3339_now(),
        complete: true,
        files_indexed,
        bytes_indexed,
        files_skipped,
        index_fingerprint: hex::encode(&fingerprint),
    }
    .write(&layout.meta)
    .at(&layout.meta)?;

    fs::remove_file(&layout.journal).at(&layout.journal)?;
    fsync_dir(&layout.dir).at(&layout.dir)?;

    Ok(ScanReport {
        files_indexed,
        bytes_indexed,
        files_skipped,
        fingerprint,
        resumed: resume_at.is_some(),
        layout,
    })
}

fn write_index(layout: &Layout, budget: usize) -> Result<([u8; 32], u64, u64)> {
    let mut sorter = Sorter::new(&layout.dir, budget).at(&layout.dir)?;
    let mut bytes = 0u64;
    for entry in journal::read(&layout.journal).at(&layout.journal)? {
        let e = entry.at(&layout.journal)?;
        let mut line = Vec::new();
        line_into(&e.hash, e.size, &e.path, &mut line);
        sorter.push(line).at(&layout.dir)?;
        bytes += e.size;
    }
    let sorted = sorter.finish().at(&layout.dir)?;
    let (fingerprint, files) = write_atomic(&layout.index, |w| {
        let mut iw = IndexWriter::new(w)?;
        for line in sorted {
            iw.write_line(&line?)?;
        }
        let records = iw.records;
        Ok((iw.finish()?, records))
    })
    .at(&layout.index)?;
    Ok((fingerprint, files, bytes))
}

fn write_hints(layout: &Layout, budget: usize) -> Result<()> {
    let mut sorter = Sorter::new(&layout.dir, budget).at(&layout.dir)?;
    for entry in journal::read(&layout.journal).at(&layout.journal)? {
        let e = entry.at(&layout.journal)?;
        sorter
            .push(hints::line(&e.path, e.size, e.mtime_nanos, &e.hash))
            .at(&layout.dir)?;
    }
    hints::write(&layout.hints, sorter.finish().at(&layout.dir)?).at(&layout.hints)
}
