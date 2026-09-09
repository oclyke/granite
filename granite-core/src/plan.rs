//! `plan`: statistics over the union of N indexes, streamed in O(N) memory.
//! Reads indexes only; never touches data files or hints.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::extsort::Sorter;
use crate::index::{IndexReader, Record};
use crate::shard::index_for_target;
use crate::{hex, pathenc};

/// Above this many shards the pairwise matrix is omitted unless asked for.
pub const PAIRWISE_AUTO_LIMIT: usize = 50;

/// How many colliding paths the report carries as examples.
const COLLISION_SAMPLE: usize = 10;

#[derive(Debug, Clone)]
pub struct Input {
    /// What the user typed; used for display only.
    pub label: String,
    pub index: PathBuf,
}

pub fn inputs_for_targets(targets: &[PathBuf]) -> Vec<Input> {
    targets
        .iter()
        .map(|t| Input {
            label: t.display().to_string(),
            index: index_for_target(t),
        })
        .collect()
}

pub struct PlanOptions {
    /// `None` applies the automatic limit.
    pub pairwise: Option<bool>,
    pub sort_budget: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShardStats {
    pub label: String,
    pub files: u64,
    pub bytes: u64,
    pub distinct_hashes: u64,
    pub unique_hashes: u64,
    pub unique_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnionStats {
    pub distinct_hashes: u64,
    pub deduplicated_bytes: u64,
    pub naive_bytes: u64,
    pub bytes_saved: u64,
    /// `naive_bytes / deduplicated_bytes`; absent when there is nothing to divide.
    pub ratio: Option<f64>,
    pub materialised_paths: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplicaBucket {
    pub copies: usize,
    pub hashes: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairOverlap {
    pub a: usize,
    pub b: usize,
    pub shared_hashes: u64,
    pub shared_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimSample {
    pub hash: String,
    /// Indexes into `PlanReport::shards`.
    pub shards: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollisionSample {
    pub path: String,
    pub claims: Vec<ClaimSample>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CollisionStats {
    pub count: u64,
    pub sample: Vec<CollisionSample>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanReport {
    pub shards: Vec<ShardStats>,
    pub union: UnionStats,
    pub replicas: Vec<ReplicaBucket>,
    pub pairwise: Option<Vec<PairOverlap>>,
    pub collisions: CollisionStats,
}

/// One record of the collision spill: encoded path, hash, shard index.
type ClaimLine = (Vec<u8>, [u8; 32], usize);

/// One hash's claim on a contested path, and the shards making it.
#[derive(Debug, Clone)]
pub struct Claim {
    pub hash: [u8; 32],
    pub shards: Vec<usize>,
}

pub struct Plan {
    pub report: PlanReport,
    pub collisions: Collisions,
}

/// Every path claimed by more than one hash. Kept on disk and streamed back
/// rather than held in a map: merging two generations of one tree collides on
/// every file that changed between them, so this list scales with the corpus.
#[derive(Debug, Default)]
pub struct Collisions {
    /// Absent when there are none; dropping it deletes the spill.
    spill: Option<tempfile::TempPath>,
    count: u64,
}

impl Collisions {
    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Streams every colliding path in ascending path order, its claims in
    /// ascending hash order.
    pub fn iter(&self) -> Result<ClaimIter> {
        let Some(spill) = &self.spill else {
            return Ok(ClaimIter::default());
        };
        let mut it = ClaimIter {
            reader: Some(BufReader::with_capacity(
                1 << 16,
                File::open(spill).at(spill)?,
            )),
            path: spill.to_path_buf(),
            next: None,
        };
        it.next = it.read_claim()?;
        Ok(it)
    }
}

#[derive(Default)]
pub struct ClaimIter {
    reader: Option<BufReader<File>>,
    path: PathBuf,
    /// The first claim of the next path, read ahead to find a group's end.
    next: Option<ClaimLine>,
}

impl ClaimIter {
    fn read_claim(&mut self) -> Result<Option<ClaimLine>> {
        let mut line = Vec::new();
        let read = match self.reader.as_mut() {
            Some(r) => r.read_until(b'\n', &mut line),
            None => return Ok(None),
        };
        if read.at(&self.path)? == 0 {
            return Ok(None);
        }
        line.pop();
        parse_claim(&line, &self.path).map(Some)
    }

    fn next_group(&mut self) -> Result<Option<(Vec<u8>, Vec<Claim>)>> {
        let Some((enc, hash, idx)) = self.next.take() else {
            return Ok(None);
        };
        let mut claims = vec![Claim {
            hash,
            shards: vec![idx],
        }];
        while let Some(c) = self.read_claim()? {
            if c.0 != enc {
                self.next = Some(c);
                break;
            }
            match claims.last_mut() {
                Some(last) if last.hash == c.1 => last.shards.push(c.2),
                _ => claims.push(Claim {
                    hash: c.1,
                    shards: vec![c.2],
                }),
            }
        }
        let path = pathenc::decode(&enc).map_err(|_| corrupt(&self.path))?;
        Ok(Some((path, claims)))
    }
}

impl Iterator for ClaimIter {
    type Item = Result<(Vec<u8>, Vec<Claim>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_group().transpose()
    }
}

struct Entry {
    line: Vec<u8>,
    idx: usize,
    rec: Record,
}

impl PartialEq for Entry {
    fn eq(&self, o: &Self) -> bool {
        self.line == o.line && self.idx == o.idx
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Entry {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (&self.line, self.idx).cmp(&(&o.line, o.idx))
    }
}

/// Yields `(input index, record)` in line-bytewise order across all inputs.
pub(crate) struct MergeStream {
    readers: Vec<IndexReader<BufReader<File>>>,
    heap: BinaryHeap<Reverse<Entry>>,
}

impl MergeStream {
    pub(crate) fn open(indexes: &[PathBuf]) -> Result<Self> {
        let mut readers = Vec::with_capacity(indexes.len());
        let mut heap = BinaryHeap::with_capacity(indexes.len());
        for (idx, path) in indexes.iter().enumerate() {
            let mut r = IndexReader::open(path)?;
            if let Some(rec) = r.next().transpose()? {
                heap.push(Reverse(Entry {
                    line: rec.line(),
                    idx,
                    rec,
                }));
            }
            readers.push(r);
        }
        Ok(MergeStream { readers, heap })
    }

    pub(crate) fn path(&self, idx: usize) -> &Path {
        self.readers[idx].path()
    }

    pub(crate) fn next(&mut self) -> Result<Option<(usize, Record)>> {
        let Some(Reverse(Entry { idx, rec, .. })) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some(next) = self.readers[idx].next().transpose()? {
            self.heap.push(Reverse(Entry {
                line: next.line(),
                idx,
                rec: next,
            }));
        }
        Ok(Some((idx, rec)))
    }
}

pub fn plan(inputs: &[Input], opts: &PlanOptions) -> Result<Plan> {
    let n = inputs.len();
    let indexes: Vec<PathBuf> = inputs.iter().map(|i| i.index.clone()).collect();
    let mut stream = MergeStream::open(&indexes)?;
    let want_pairwise = opts.pairwise.unwrap_or(n <= PAIRWISE_AUTO_LIMIT);

    let tmp = std::env::temp_dir();
    let mut by_path = Sorter::new(&tmp, opts.sort_budget).at(&tmp)?;

    let mut shards: Vec<ShardStats> = inputs
        .iter()
        .map(|i| ShardStats {
            label: i.label.clone(),
            files: 0,
            bytes: 0,
            distinct_hashes: 0,
            unique_hashes: 0,
            unique_bytes: 0,
        })
        .collect();
    let mut replicas: Vec<ReplicaBucket> = (1..=n)
        .map(|c| ReplicaBucket {
            copies: c,
            hashes: 0,
            bytes: 0,
        })
        .collect();
    let mut pair_counts = vec![(0u64, 0u64); n * n];
    let mut union = UnionStats {
        distinct_hashes: 0,
        deduplicated_bytes: 0,
        naive_bytes: 0,
        bytes_saved: 0,
        ratio: None,
        materialised_paths: 0,
    };

    // Per-hash group state; a group is the run of records sharing a hash.
    let mut group: Option<([u8; 32], u64, usize)> = None;
    let mut members: Vec<usize> = Vec::new();
    let mut last_path: Option<Vec<u8>> = None;

    let mut close_group = |members: &mut Vec<usize>, size: u64, shards: &mut Vec<ShardStats>| {
        members.sort_unstable();
        members.dedup();
        union.distinct_hashes += 1;
        union.deduplicated_bytes += size;
        let bucket = &mut replicas[members.len() - 1];
        bucket.hashes += 1;
        bucket.bytes += size;
        for &m in members.iter() {
            shards[m].distinct_hashes += 1;
        }
        if let [only] = members[..] {
            shards[only].unique_hashes += 1;
            shards[only].unique_bytes += size;
        }
        if want_pairwise {
            for (i, &a) in members.iter().enumerate() {
                for &b in &members[i + 1..] {
                    let cell = &mut pair_counts[a * n + b];
                    cell.0 += 1;
                    cell.1 += size;
                }
            }
        }
        members.clear();
    };

    while let Some((idx, rec)) = stream.next()? {
        shards[idx].files += 1;
        shards[idx].bytes += rec.size;
        match group {
            Some((h, size, first_idx)) if h == rec.hash => {
                if size != rec.size {
                    return Err(Error::Malformed {
                        path: stream.path(idx).to_path_buf(),
                        line: 0,
                        msg: format!(
                            "hash {} has size {} here but size {size} in {}",
                            hex::encode(&h),
                            rec.size,
                            stream.path(first_idx).display()
                        ),
                    });
                }
            }
            Some((_, size, _)) => {
                close_group(&mut members, size, &mut shards);
                group = Some((rec.hash, rec.size, idx));
                last_path = None;
            }
            None => group = Some((rec.hash, rec.size, idx)),
        }
        members.push(idx);
        // Same (hash, path) from several shards arrives adjacent, so this counts
        // distinct pairs.
        if last_path.as_deref() != Some(&rec.path[..]) {
            union.materialised_paths += 1;
            last_path = Some(rec.path.clone());
        }
        let mut line = pathenc::encode(&rec.path);
        line.push(b'\t');
        line.extend_from_slice(hex::encode(&rec.hash).as_bytes());
        line.push(b'\t');
        line.extend_from_slice(idx.to_string().as_bytes());
        by_path.push(line).at(&tmp)?;
    }
    if let Some((_, size, _)) = group {
        close_group(&mut members, size, &mut shards);
    }

    union.naive_bytes = shards.iter().map(|s| s.bytes).sum();
    union.bytes_saved = union.naive_bytes - union.deduplicated_bytes;
    if union.deduplicated_bytes > 0 {
        union.ratio = Some(union.naive_bytes as f64 / union.deduplicated_bytes as f64);
    }
    let pairwise = want_pairwise.then(|| {
        let mut v = Vec::new();
        for a in 0..n {
            for b in a + 1..n {
                let (h, bytes) = pair_counts[a * n + b];
                v.push(PairOverlap {
                    a,
                    b,
                    shared_hashes: h,
                    shared_bytes: bytes,
                });
            }
        }
        v
    });

    let (collisions, sample) = find_collisions(by_path.finish().at(&tmp)?, &tmp)?;
    let report = PlanReport {
        shards,
        union,
        replicas,
        pairwise,
        collisions: CollisionStats {
            count: collisions.count(),
            sample,
        },
    };
    Ok(Plan { report, collisions })
}

fn corrupt(path: &Path) -> Error {
    Error::Malformed {
        path: path.to_path_buf(),
        line: 0,
        msg: "corrupt sort spill".into(),
    }
}

fn parse_claim(line: &[u8], tmp: &Path) -> Result<ClaimLine> {
    let bad = || corrupt(tmp);
    let t2 = line.iter().rposition(|&b| b == b'\t').ok_or_else(bad)?;
    let t1 = line[..t2]
        .iter()
        .rposition(|&b| b == b'\t')
        .ok_or_else(bad)?;
    let hash = hex::decode_hash(&line[t1 + 1..t2]).ok_or_else(bad)?;
    let idx: usize = std::str::from_utf8(&line[t2 + 1..])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    Ok((line[..t1].to_vec(), hash, idx))
}

/// Copies a colliding group back out in the same `encoded_path \t hex \t shard`
/// form it arrived in, so `ClaimIter` can re-read it with `parse_claim`.
fn emit_collision(
    enc: &[u8],
    claims: &[Claim],
    w: &mut impl Write,
    sample: &mut Vec<CollisionSample>,
    tmp: &Path,
) -> Result<()> {
    if sample.len() < COLLISION_SAMPLE {
        let path = pathenc::decode(enc).map_err(|_| corrupt(tmp))?;
        sample.push(CollisionSample {
            path: pathenc::display(&path),
            claims: claims
                .iter()
                .map(|c| ClaimSample {
                    hash: hex::encode(&c.hash),
                    shards: c.shards.clone(),
                })
                .collect(),
        });
    }
    for c in claims {
        for &i in &c.shards {
            w.write_all(enc).at(tmp)?;
            writeln!(w, "\t{}\t{i}", hex::encode(&c.hash)).at(tmp)?;
        }
    }
    Ok(())
}

/// Lines are `encoded_path \t hex \t shard`, sorted, so a path's claims are
/// adjacent and in ascending hash order. Only one group and the sample are
/// held at a time; the rest goes straight back to disk.
fn find_collisions(
    sorted: impl Iterator<Item = std::io::Result<Vec<u8>>>,
    tmp: &Path,
) -> Result<(Collisions, Vec<CollisionSample>)> {
    let file = tempfile::Builder::new()
        .prefix(".granite-collisions.")
        .tempfile_in(tmp)
        .at(tmp)?;
    let mut count = 0u64;
    let mut sample = Vec::new();
    {
        let mut w = BufWriter::with_capacity(1 << 16, file.as_file());
        let mut cur: Option<(Vec<u8>, Vec<Claim>)> = None;
        for line in sorted {
            let (enc, hash, idx) = parse_claim(&line.at(tmp)?, tmp)?;
            match &mut cur {
                Some((p, claims)) if *p == enc => match claims.last_mut() {
                    Some(c) if c.hash == hash => c.shards.push(idx),
                    _ => claims.push(Claim {
                        hash,
                        shards: vec![idx],
                    }),
                },
                _ => {
                    if let Some((p, claims)) = cur.take().filter(|(_, c)| c.len() > 1) {
                        count += 1;
                        emit_collision(&p, &claims, &mut w, &mut sample, tmp)?;
                    }
                    cur = Some((
                        enc,
                        vec![Claim {
                            hash,
                            shards: vec![idx],
                        }],
                    ));
                }
            }
        }
        if let Some((p, claims)) = cur.filter(|(_, c)| c.len() > 1) {
            count += 1;
            emit_collision(&p, &claims, &mut w, &mut sample, tmp)?;
        }
        w.flush().at(tmp)?;
    }
    let spill = (count > 0).then(|| file.into_temp_path());
    Ok((Collisions { spill, count }, sample))
}
