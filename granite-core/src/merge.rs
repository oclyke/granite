//! `merge`: materialise the union of N shards (§7.3).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, IoContext, Result};
use crate::extsort::Sorter;
use crate::fsutil::{self, fsync_dir, write_atomic};
use crate::hash::{BUF_SIZE, copy_hashing, hash_file};
use crate::hex;
use crate::index::{IndexWriter, line_into};
use crate::plan::{self, Input, Plan, PlanOptions};
use crate::shard::Layout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLayout {
    Paths,
    Hash,
}

pub struct MergeOptions {
    pub layout: OutputLayout,
    pub link: bool,
    pub dedupe_links: bool,
    pub index_dir: Option<PathBuf>,
    pub pairwise: Option<bool>,
    pub sort_budget: usize,
}

pub struct MergePlan {
    pub plan: Plan,
    roots: Vec<PathBuf>,
    indexes: Vec<PathBuf>,
}

impl MergePlan {
    /// Path collisions block the `paths` layout: a path is a location in the
    /// output, and one location holds one file. The user resolves them by
    /// renaming in a source shard and rescanning it. The `hash` layout has no
    /// path collisions.
    pub fn blocked_by_collisions(&self, layout: OutputLayout) -> bool {
        layout == OutputLayout::Paths && !self.plan.collisions.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub path: Vec<u8>,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct MergeReport {
    pub written: u64,
    pub linked: u64,
    pub existing: u64,
    pub bytes_written: u64,
    pub failed: Vec<Failure>,
    pub fingerprint: [u8; 32],
}

/// Canonicalises a path that may not exist yet by resolving its deepest
/// existing ancestor.
fn canonical_lenient(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|n| n.to_owned()) else {
            return Err(Error::Usage(format!(
                "{} has no existing ancestor",
                path.display()
            )));
        };
        rest.push(name);
        if !existing.pop() || existing.as_os_str().is_empty() {
            existing = PathBuf::from(".");
        }
    }
    let mut out = fs::canonicalize(&existing).at(&existing)?;
    out.extend(rest.iter().rev());
    Ok(out)
}

pub fn prepare(roots: &[PathBuf], into: &Path, opts: &MergeOptions) -> Result<MergePlan> {
    if roots.is_empty() {
        return Err(Error::Usage("merge needs at least one source shard".into()));
    }
    let into_c = canonical_lenient(into)?;
    let mut indexes = Vec::with_capacity(roots.len());
    for root in roots {
        if !root.is_dir() {
            return Err(Error::Usage(format!(
                "{} is not a shard root directory",
                root.display()
            )));
        }
        let root_c = fs::canonicalize(root).at(root)?;
        if into_c.starts_with(&root_c) || root_c.starts_with(&into_c) {
            return Err(Error::Usage(format!(
                "output {} overlaps source shard {}",
                into.display(),
                root.display()
            )));
        }
        let layout = Layout::resolve(root, opts.index_dir.as_deref(), None).at(root)?;
        if !layout.index.is_file() {
            return Err(Error::Usage(format!(
                "{} has no index at {}\nhint:  run `granite scan` on it first",
                root.display(),
                layout.index.display()
            )));
        }
        indexes.push(layout.index);
    }
    let inputs: Vec<Input> = roots
        .iter()
        .zip(&indexes)
        .map(|(r, i)| Input {
            label: r.display().to_string(),
            index: i.clone(),
        })
        .collect();
    let plan = plan::plan(
        &inputs,
        &PlanOptions {
            pairwise: opts.pairwise,
            sort_budget: opts.sort_budget,
        },
    )?;

    Ok(MergePlan {
        plan,
        roots: roots.to_vec(),
        indexes,
    })
}

fn hash_layout_path(hash: &[u8; 32]) -> Vec<u8> {
    let hex = hex::encode(hash);
    format!("objects/{}/{hex}", &hex[..2]).into_bytes()
}

enum Outcome {
    Written(u64),
    Linked,
    Existing,
    Failed(String),
}

/// Directory listings cached for the alias check; cleared wholesale when full.
const DIR_CACHE_LIMIT: usize = 4096;

/// Device-cache flush cadence. Each file gets a plain fsync before its rename;
/// the drive's own cache is flushed this often so a power loss can only take
/// back a bounded number of already-renamed files.
const DEVICE_FLUSH_EVERY_FILES: u64 = 1000;
const DEVICE_FLUSH_EVERY: Duration = Duration::from_secs(5);

struct Writer<'a> {
    into: &'a Path,
    opts: &'a MergeOptions,
    buf: Vec<u8>,
    warned_cross_device: bool,
    dir_cache: HashMap<PathBuf, HashSet<Vec<u8>>>,
    out_dir: fs::File,
    since_flush: u64,
    last_flush: Instant,
    log: &'a mut dyn FnMut(&str),
}

impl Writer<'_> {
    fn flushed_write(&mut self) -> std::io::Result<()> {
        self.since_flush += 1;
        if self.since_flush >= DEVICE_FLUSH_EVERY_FILES
            || self.last_flush.elapsed() >= DEVICE_FLUSH_EVERY
        {
            self.flush_device()?;
        }
        Ok(())
    }

    fn flush_device(&mut self) -> std::io::Result<()> {
        fsutil::flush_device(&self.out_dir)?;
        self.since_flush = 0;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Whether `readdir` on `parent` returns exactly these name bytes. A lookup
    /// can succeed for a name the directory does not literally contain when the
    /// filesystem folds case or Unicode normalisation; such a name must not be
    /// indexed as if it had been written.
    fn listed(&mut self, parent: &Path, name: &[u8]) -> std::io::Result<bool> {
        use std::os::unix::ffi::OsStrExt;
        if !self.dir_cache.contains_key(parent) {
            if self.dir_cache.len() >= DIR_CACHE_LIMIT {
                self.dir_cache.clear();
            }
            let names = fs::read_dir(parent)?
                .map(|e| e.map(|e| e.file_name().as_bytes().to_vec()))
                .collect::<std::io::Result<HashSet<_>>>()?;
            self.dir_cache.insert(parent.to_path_buf(), names);
        }
        Ok(self.dir_cache[parent].contains(name))
    }

    fn note_written(&mut self, parent: &Path, name: &[u8]) {
        if let Some(names) = self.dir_cache.get_mut(parent) {
            names.insert(name.to_vec());
        }
    }

    fn place(
        &mut self,
        out_rel: &[u8],
        src: &Path,
        hash: &[u8; 32],
        size: u64,
        twin: Option<&Path>,
    ) -> Outcome {
        let out = self.into.join(bytes_path(out_rel));
        match self.place_inner(&out, src, hash, size, twin) {
            Ok(o) => o,
            Err(e) => Outcome::Failed(e.to_string()),
        }
    }

    fn place_inner(
        &mut self,
        out: &Path,
        src: &Path,
        hash: &[u8; 32],
        size: u64,
        twin: Option<&Path>,
    ) -> std::io::Result<Outcome> {
        let parent = out.parent().unwrap_or(self.into);
        let name = out
            .file_name()
            .map(|n| n.as_encoded_bytes())
            .unwrap_or_default();
        if let Ok(meta) = fs::symlink_metadata(out) {
            if !self.listed(parent, name)? {
                return Ok(Outcome::Failed(
                    "the filesystem maps this name onto an existing entry spelled differently (case or Unicode normalisation); not written".into(),
                ));
            }
            if !meta.is_file() {
                return Ok(Outcome::Failed("exists and is not a regular file".into()));
            }
            if meta.len() == size && hash_file(out, &mut self.buf)?.0 == *hash {
                return Ok(Outcome::Existing);
            }
            return Ok(Outcome::Failed(
                "exists with different content; not overwritten".into(),
            ));
        }
        fs::create_dir_all(parent)?;

        // Same hash already materialised in this output: link to it. Its bytes were
        // verified when it was written, and a hardlink shares the inode.
        if let Some(twin) = twin
            && self.opts.dedupe_links
            && fs::hard_link(twin, out).is_ok()
        {
            self.note_written(parent, name);
            return Ok(Outcome::Linked);
        }

        if self.opts.link {
            let same_device = fs::metadata(src)?.dev() == fs::metadata(parent)?.dev();
            if same_device {
                let tmp = tempfile::Builder::new()
                    .prefix(".granite-tmp.")
                    .make_in(parent, |p| fs::hard_link(src, p))?;
                if hash_file(tmp.path(), &mut self.buf)?.0 == *hash {
                    tmp.persist(out).map_err(|e| e.error)?;
                    self.note_written(parent, name);
                    return Ok(Outcome::Linked);
                }
                return Ok(Outcome::Failed(format!(
                    "{} does not hash to {}",
                    src.display(),
                    hex::encode(hash)
                )));
            } else if !self.warned_cross_device {
                self.warned_cross_device = true;
                (self.log)(
                    "warning: --link requested but source and output are on different devices; copying",
                );
            }
        }

        let src_meta = fs::metadata(src)?;
        let tmp = tempfile::NamedTempFile::with_prefix_in(".granite-tmp.", parent)?;
        let (got, n) = {
            let mut w = std::io::BufWriter::with_capacity(BUF_SIZE, tmp.as_file());
            let r = copy_hashing(src, &mut w, &mut self.buf)?;
            w.flush()?;
            r
        };
        if got != *hash {
            let (_, kept) = tmp.keep().map_err(|e| e.error)?;
            return Ok(Outcome::Failed(format!(
                "{} hashed to {} but the index says {}; partial copy left at {}",
                src.display(),
                hex::encode(&got),
                hex::encode(hash),
                kept.display()
            )));
        }
        let file = tmp.as_file();
        file.set_permissions(fs::Permissions::from_mode(src_meta.mode() & 0o777))?;
        if let Ok(mtime) = src_meta.modified() {
            file.set_modified(mtime)?;
        }
        fsutil::fsync(file)?;
        tmp.persist(out).map_err(|e| e.error)?;
        self.note_written(parent, name);
        self.flushed_write()?;
        Ok(Outcome::Written(n))
    }
}

fn bytes_path(b: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(b))
}

pub fn execute(
    mp: &MergePlan,
    into: &Path,
    opts: &MergeOptions,
    log: &mut dyn FnMut(&str),
) -> Result<MergeReport> {
    if mp.blocked_by_collisions(opts.layout) {
        return Err(Error::Usage(format!(
            "{} paths are claimed by more than one file; resolve them before merging",
            mp.plan.collisions.len()
        )));
    }
    let out_layout = Layout::in_shard(into);
    fs::create_dir_all(&out_layout.dir).at(&out_layout.dir)?;
    let mut out_index = Sorter::new(&out_layout.dir, opts.sort_budget).at(&out_layout.dir)?;
    let paths_file = out_layout.dir.join("paths");
    let mut paths_index = match opts.layout {
        OutputLayout::Hash => {
            Some(Sorter::new(&out_layout.dir, opts.sort_budget).at(&out_layout.dir)?)
        }
        OutputLayout::Paths => None,
    };

    let mut report = MergeReport::default();
    let mut writer = Writer {
        into,
        opts,
        buf: vec![0u8; BUF_SIZE],
        warned_cross_device: false,
        dir_cache: HashMap::new(),
        out_dir: fs::File::open(into).at(into)?,
        since_flush: 0,
        last_flush: Instant::now(),
        log,
    };
    let mut stream = plan::MergeStream::open(&mp.indexes)?;

    // Group state: the source chosen for the current hash and the first output
    // path holding it (for dedupe links).
    let mut cur: Option<([u8; 32], PathBuf, Option<PathBuf>)> = None;
    let mut last_path: Option<Vec<u8>> = None;

    while let Some((idx, rec)) = stream.next()? {
        let new_group = cur.as_ref().is_none_or(|(h, _, _)| *h != rec.hash);
        if new_group {
            cur = Some((rec.hash, mp.roots[idx].join(bytes_path(&rec.path)), None));
        } else if last_path.as_deref() == Some(&rec.path[..]) {
            continue;
        }
        last_path = Some(rec.path.clone());
        if let Some(pi) = paths_index.as_mut() {
            pi.push(rec.line()).at(&paths_file)?;
        }

        let out_rel = match opts.layout {
            OutputLayout::Hash => {
                if !new_group {
                    continue;
                }
                hash_layout_path(&rec.hash)
            }
            OutputLayout::Paths => rec.path.clone(),
        };
        let Some((hash, src, first_out)) = cur.as_mut() else {
            continue;
        };
        match writer.place(&out_rel, src, hash, rec.size, first_out.as_deref()) {
            Outcome::Failed(reason) => {
                report.failed.push(Failure {
                    path: out_rel,
                    reason,
                });
                continue;
            }
            Outcome::Written(n) => {
                report.written += 1;
                report.bytes_written += n;
            }
            Outcome::Linked => report.linked += 1,
            Outcome::Existing => report.existing += 1,
        }
        if first_out.is_none() {
            *first_out = Some(into.join(bytes_path(&out_rel)));
        }
        let mut line = Vec::new();
        line_into(hash, rec.size, &out_rel, &mut line);
        out_index.push(line).at(&out_layout.dir)?;
    }

    writer.flush_device().at(into)?;

    if let Some(pi) = paths_index {
        let sorted = pi.finish().at(&out_layout.dir)?;
        write_atomic(&paths_file, |w| {
            let mut iw = IndexWriter::new(w)?;
            for line in sorted {
                iw.write_line(&line?)?;
            }
            iw.finish()
        })
        .at(&paths_file)?;
    }
    let sorted = out_index.finish().at(&out_layout.dir)?;
    report.fingerprint = write_atomic(&out_layout.index, |w| {
        let mut iw = IndexWriter::new(w)?;
        for line in sorted {
            iw.write_line(&line?)?;
        }
        iw.finish()
    })
    .at(&out_layout.index)?;
    fsync_dir(into).at(into)?;
    Ok(report)
}
