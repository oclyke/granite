//! `plan`: statistics over the union of N indexes, streamed in O(N) memory.
//! Reads indexes only; never touches data files or hints.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::File;
use std::io::BufReader;
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
pub struct Collisions {
    pub count: u64,
    pub sample: Vec<CollisionSample>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanReport {
    pub shards: Vec<ShardStats>,
    pub union: UnionStats,
    pub replicas: Vec<ReplicaBucket>,
    pub pairwise: Option<Vec<PairOverlap>>,
    pub collisions: Collisions,
}

/// One hash's claim on a contested path, and the shards making it.
#[derive(Debug, Clone)]
pub struct Claim {
    pub hash: [u8; 32],
    pub shards: Vec<usize>,
}

pub struct Plan {
    pub report: PlanReport,
    /// Every path claimed by more than one hash, claims in ascending hash order.
    pub collisions: BTreeMap<Vec<u8>, Vec<Claim>>,
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

    let collisions = find_collisions(by_path.finish().at(&tmp)?, &tmp)?;
    let sample = collisions
        .iter()
        .take(COLLISION_SAMPLE)
        .map(|(p, claims)| CollisionSample {
            path: pathenc::display(p),
            claims: claims
                .iter()
                .map(|c| ClaimSample {
                    hash: hex::encode(&c.hash),
                    shards: c.shards.clone(),
                })
                .collect(),
        })
        .collect();
    let report = PlanReport {
        shards,
        union,
        replicas,
        pairwise,
        collisions: Collisions {
            count: collisions.len() as u64,
            sample,
        },
    };
    Ok(Plan { report, collisions })
}

/// Lines are `encoded_path \t hex \t shard`, sorted, so a path's claims are
/// adjacent and in ascending hash order.
fn find_collisions(
    sorted: impl Iterator<Item = std::io::Result<Vec<u8>>>,
    tmp: &Path,
) -> Result<BTreeMap<Vec<u8>, Vec<Claim>>> {
    let corrupt = || Error::Malformed {
        path: tmp.to_path_buf(),
        line: 0,
        msg: "corrupt sort spill".into(),
    };
    let mut out = BTreeMap::new();
    let mut cur: Option<(Vec<u8>, Vec<Claim>)> = None;
    let mut flush = |cur: Option<(Vec<u8>, Vec<Claim>)>| -> Result<()> {
        if let Some((p, claims)) = cur.filter(|(_, c)| c.len() > 1) {
            out.insert(pathenc::decode(&p).map_err(|_| corrupt())?, claims);
        }
        Ok(())
    };
    for line in sorted {
        let line = line.at(tmp)?;
        let t2 = line.iter().rposition(|&b| b == b'\t').ok_or_else(corrupt)?;
        let t1 = line[..t2]
            .iter()
            .rposition(|&b| b == b'\t')
            .ok_or_else(corrupt)?;
        let (enc, hex_part, idx_part) = (&line[..t1], &line[t1 + 1..t2], &line[t2 + 1..]);
        let hash = hex::decode_hash(hex_part).ok_or_else(corrupt)?;
        let idx: usize = std::str::from_utf8(idx_part)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(corrupt)?;
        if cur.as_ref().is_none_or(|(p, _)| p.as_slice() != enc) {
            flush(cur.take())?;
            cur = Some((enc.to_vec(), Vec::new()));
        }
        let claims = &mut cur.as_mut().ok_or_else(corrupt)?.1;
        match claims.last_mut() {
            Some(c) if c.hash == hash => c.shards.push(idx),
            _ => claims.push(Claim {
                hash,
                shards: vec![idx],
            }),
        }
    }
    flush(cur)?;
    Ok(out)
}
