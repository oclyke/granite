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
use crate::index::{IndexReader, IndexWriter, Record, line_into};
use crate::pathenc;
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
    /// Places every source path under this relative directory in the output.
    pub under: Option<Vec<u8>>,
    pub pairwise: Option<bool>,
    pub sort_budget: usize,
}

pub struct MergePlan {
    pub plan: Plan,
    roots: Vec<PathBuf>,
    inputs: Vec<Input>,
    /// Input 0 is the output's own existing index. Its files are already in
    /// place, and index 0 sorts it first among records with identical lines.
    target: bool,
}

impl MergePlan {
    /// Path collisions block the `paths` layout: a path is a location in the
    /// output, and one location holds one file. The user resolves them by
    /// renaming in a source shard and rescanning it. The `hash` layout has no
    /// path collisions.
    pub fn blocked_by_collisions(&self, layout: OutputLayout) -> bool {
        layout == OutputLayout::Paths && !self.plan.collisions.is_empty()
    }

    /// Where input `idx`'s file for an output path lives.
    pub fn source(&self, idx: usize, path: &[u8]) -> PathBuf {
        self.roots[idx].join(bytes_path(&path[self.inputs[idx].prefix.len()..]))
    }

    fn held(&self, idx: usize) -> bool {
        self.target && idx == 0
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
    let prefix = match &opts.under {
        None => Vec::new(),
        Some(p) => {
            let mut p = p.as_slice();
            while let Some(rest) = p.strip_suffix(b"/") {
                p = rest;
            }
            pathenc::validate_relative(p)
                .map_err(|e| Error::Usage(format!("--under {}: {e}", pathenc::display(p))))?;
            [p, b"/"].concat()
        }
    };
    let out = Layout::in_shard(into);
    let target = out.index.is_file();
    let mut all_roots = Vec::with_capacity(roots.len() + 1);
    let mut inputs = Vec::with_capacity(roots.len() + 1);
    if target {
        let hashed = out.dir.join("paths").is_file();
        if hashed != (opts.layout == OutputLayout::Hash) {
            return Err(Error::Usage(format!(
                "{} already holds a merge in the {} layout; merge into it with the same --layout",
                into.display(),
                if hashed { "hash" } else { "paths" }
            )));
        }
        all_roots.push(into.to_path_buf());
        inputs.push(Input {
            label: into.display().to_string(),
            index: out.index,
            prefix: Vec::new(),
        });
    }
    for root in roots {
        if !root.is_dir() {
            return Err(Error::Usage(format!(
                "{} is not a shard root directory",
                root.display()
            )));
        }
        let root_c = fs::canonicalize(root).at(root)?;
        if target && root_c == into_c {
            continue;
        }
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
        all_roots.push(root.clone());
        inputs.push(Input {
            label: root.display().to_string(),
            index: layout.index,
            prefix: prefix.clone(),
        });
    }
    let plan = plan::plan(
        &inputs,
        &PlanOptions {
            pairwise: opts.pairwise,
            sort_budget: opts.sort_budget,
        },
    )?;

    Ok(MergePlan {
        plan,
        roots: all_roots,
        inputs,
        target,
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
    progress: &mut dyn FnMut(u64),
) -> Result<MergeReport> {
    if mp.blocked_by_collisions(opts.layout) {
        return Err(Error::Usage(format!(
            "{} paths are claimed by more than one file; resolve them before merging",
            mp.plan.collisions.count()
        )));
    }
    let out_layout = Layout::in_shard(into);
    fs::create_dir_all(&out_layout.dir).at(&out_layout.dir)?;
    let mut out_index = Sorter::new(&out_layout.dir, opts.sort_budget).at(&out_layout.dir)?;
    let paths_file = out_layout.dir.join("paths");
    let mut paths_index = match opts.layout {
        OutputLayout::Hash => {
            let mut s = Sorter::new(&out_layout.dir, opts.sort_budget).at(&out_layout.dir)?;
            if mp.target {
                for rec in IndexReader::open(&paths_file)? {
                    s.push(rec?.line()).at(&paths_file)?;
                }
            }
            Some(s)
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

    // One hash's records, buffered so a copy already held by the output is
    // known before any path of that hash is placed.
    let mut place_group = |group: &[(usize, Record)]| -> Result<()> {
        let Some((_, head)) = group.first() else {
            return Ok(());
        };
        let mut first_out = group
            .iter()
            .find(|(i, _)| mp.held(*i))
            .map(|(_, r)| into.join(bytes_path(&r.path)));
        let held = first_out.is_some();
        let mut last_path: Option<&[u8]> = None;
        for (n, (i, rec)) in group.iter().enumerate() {
            if last_path == Some(&rec.path[..]) {
                continue;
            }
            last_path = Some(&rec.path);
            let mine = mp.held(*i);
            if !mine && let Some(pi) = paths_index.as_mut() {
                pi.push(rec.line()).at(&paths_file)?;
            }
            let out_rel = match opts.layout {
                OutputLayout::Paths => rec.path.clone(),
                OutputLayout::Hash if mine || (!held && n == 0) => hash_layout_path(&head.hash),
                OutputLayout::Hash => continue,
            };
            progress(head.size);
            let outcome = if mine {
                Outcome::Existing
            } else {
                writer.place(
                    &out_rel,
                    &mp.source(*i, &rec.path),
                    &head.hash,
                    head.size,
                    first_out.as_deref(),
                )
            };
            match outcome {
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
                first_out = Some(into.join(bytes_path(&out_rel)));
            }
            let mut line = Vec::new();
            line_into(&head.hash, head.size, &out_rel, &mut line);
            out_index.push(line).at(&out_layout.dir)?;
        }
        Ok(())
    };

    let mut stream = plan::MergeStream::open(&mp.inputs)?;
    let mut group: Vec<(usize, Record)> = Vec::new();
    while let Some(item) = stream.next()? {
        if group.first().is_some_and(|(_, r)| r.hash != item.1.hash) {
            place_group(&group)?;
            group.clear();
        }
        group.push(item);
    }
    place_group(&group)?;

    writer.flush_device().at(into)?;

    if let Some(pi) = paths_index {
        let sorted = pi.finish().at(&out_layout.dir)?;
        write_atomic(&paths_file, |w| {
            let mut iw = IndexWriter::new(w)?;
            let mut prev = None;
            for line in sorted {
                let line = line?;
                if prev.as_ref() != Some(&line) {
                    iw.write_line(&line)?;
                }
                prev = Some(line);
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
