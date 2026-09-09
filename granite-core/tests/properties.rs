use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use granite_core::DEFAULT_SORT_BUDGET;
use granite_core::index::IndexReader;
use granite_core::merge::{self, MergeOptions, MergeReport, OutputLayout};
use granite_core::pathenc;
use granite_core::plan::{self, Input, PlanOptions};
use granite_core::scan::{self, ScanOptions};
use granite_core::verify::{self, VerifyOptions};
use proptest::prelude::*;

fn bytes_path(b: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(b))
}

/// Creates files; names the filesystem rejects (invalid UTF-8 on APFS) are skipped.
fn tree(root: &Path, files: &[(&[u8], &[u8])]) {
    fs::create_dir_all(root).unwrap();
    for (path, content) in files {
        let p = root.join(bytes_path(path));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = fs::write(&p, content);
    }
}

fn scan_opts(force: bool) -> ScanOptions {
    ScanOptions {
        index_dir: None,
        name: None,
        force,
        jobs: 2,
        sort_budget: DEFAULT_SORT_BUDGET,
    }
}

fn do_scan(root: &Path, force: bool) -> Vec<u8> {
    scan::scan(root, &scan_opts(force), &mut |_| {}).unwrap();
    fs::read(root.join(".granite/index")).unwrap()
}

fn merge_opts() -> MergeOptions {
    MergeOptions {
        layout: OutputLayout::Paths,
        link: false,
        dedupe_links: true,
        index_dir: None,
        pairwise: None,
        sort_budget: DEFAULT_SORT_BUDGET,
    }
}

/// The only tolerated failure is the NFC/NFD pair on a normalisation-folding
/// filesystem (APFS), where the second spelling cannot be materialised.
fn do_merge(roots: &[PathBuf], into: &Path) -> MergeReport {
    let opts = merge_opts();
    let mp = merge::prepare(roots, into, &opts).unwrap();
    let r = merge::execute(&mp, into, &opts, &mut |_| {}).unwrap();
    for f in &r.failed {
        assert!(
            f.path.starts_with(b"unicode/") && f.reason.contains("normalisation"),
            "{f:?}"
        );
    }
    r
}

fn read_tree(root: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .as_os_str()
                .as_bytes()
                .to_vec();
            (rel, fs::read(e.path()).unwrap())
        })
        .collect()
}

fn records(index: &Path) -> Vec<granite_core::index::Record> {
    IndexReader::open(index)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    shards: Vec<PathBuf>,
    base: PathBuf,
}

/// Three overlapping shards covering every fixture case from §8.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().to_path_buf();
    let a = base.join("a");
    let b = base.join("b");
    let c = base.join("c");
    tree(
        &a,
        &[
            (b"photos/2019/IMG_001.jpg", b"jpeg-one"),
            (b"sorted/one.jpg", b"jpeg-one"),
            (b"dup/again/one.jpg", b"jpeg-one"),
            (b"notes.txt", b"notes version A"),
            (b"zero/a", b""),
            (b"zero/b", b""),
            (b"odd/new\nline\ttab.txt", b"odd name"),
            (b"odd/back\\slash", b"backslash"),
            (b"odd/bad\xff\xfe.bin", b"invalid utf8"),
            (b"unicode/caf\xc3\xa9.txt", b"accent"),
            (
                b"deep/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/leaf",
                b"deep",
            ),
        ],
    );
    tree(
        &b,
        &[
            (b"photos/2019/IMG_001.jpg", b"jpeg-one"),
            (b"notes-b.txt", b"notes version B"),
            (b"zero/c", b""),
            (b"unicode/cafe\xcc\x81.txt", b"accent"),
            (b"only-b.bin", b"exclusive to b"),
        ],
    );
    tree(
        &c,
        &[
            (b"unrelated/x", b"no overlap at all"),
            (b"unrelated/y", b"none"),
            (b"notes.txt", b"notes version A"),
        ],
    );
    tree(&base.join("empty"), &[]);
    for s in [&a, &b, &c] {
        do_scan(s, true);
    }
    do_scan(&base.join("empty"), true);
    Fixture {
        _dir: dir,
        shards: vec![a, b, c],
        base,
    }
}

#[test]
fn merge_is_order_independent() {
    let f = fixture();
    let perms = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut outputs = Vec::new();
    for (i, p) in perms.iter().enumerate() {
        let roots: Vec<PathBuf> = p.iter().map(|&k| f.shards[k].clone()).collect();
        let out = f.base.join(format!("out{i}"));
        do_merge(&roots, &out);
        outputs.push(read_tree(&out));
    }
    for o in &outputs[1..] {
        assert_eq!(o, &outputs[0]);
    }
    assert_eq!(
        outputs[0].get(&b"notes.txt"[..]).map(Vec::as_slice),
        Some(&b"notes version A"[..])
    );
}

#[test]
fn merge_is_associative() {
    let f = fixture();
    let ab = f.base.join("ab");
    do_merge(&f.shards[..2], &ab);
    let abc_nested = f.base.join("abc_nested");
    do_merge(&[ab.clone(), f.shards[2].clone()], &abc_nested);
    let abc_flat = f.base.join("abc_flat");
    do_merge(&f.shards, &abc_flat);
    assert_eq!(read_tree(&abc_nested), read_tree(&abc_flat));
}

#[test]
fn merge_is_idempotent() {
    let f = fixture();
    let out = f.base.join("out");
    let first = do_merge(&f.shards, &out);
    assert!(first.written + first.linked > 0);
    let before = read_tree(&out);
    let again = do_merge(&f.shards, &out);
    assert_eq!((again.written, again.linked), (0, 0));
    assert_eq!(again.existing, first.written + first.linked);
    assert_eq!(read_tree(&out), before);

    let mut roots = f.shards.clone();
    roots.push(out.clone());
    let out2 = f.base.join("out2");
    do_merge(&roots, &out2);
    assert_eq!(read_tree(&out2), read_tree(&out));
}

#[test]
fn index_is_deterministic() {
    let f = fixture();
    let a = &f.shards[0];
    let first = fs::read(a.join(".granite/index")).unwrap();
    assert_eq!(do_scan(a, true), first);

    let copy = f.base.join("elsewhere/deeper/a-copy");
    for (p, content) in read_tree(a) {
        if p.starts_with(b".granite/") {
            continue;
        }
        let dst = copy.join(bytes_path(&p));
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(dst, content).unwrap();
    }
    assert_eq!(do_scan(&copy, true), first);
    assert_eq!(
        granite_core::index::fingerprint(&copy.join(".granite/index")).unwrap(),
        granite_core::index::fingerprint(&a.join(".granite/index")).unwrap()
    );
}

#[test]
fn hints_are_inert() {
    let f = fixture();
    let a = &f.shards[0];
    let forced = do_scan(a, true);
    assert!(a.join(".granite/hints").exists());
    assert_eq!(do_scan(a, false), forced);
    fs::write(a.join("notes.txt"), b"changed after hints").unwrap();
    let hinted = do_scan(a, false);
    assert_ne!(hinted, forced);
    assert_eq!(do_scan(a, true), hinted);
}

#[test]
fn plan_matches_merge() {
    let f = fixture();
    let mut targets = f.shards.clone();
    targets.push(f.base.join("empty"));
    let inputs: Vec<Input> = targets
        .iter()
        .map(|t| Input {
            label: String::new(),
            index: t.join(".granite/index"),
        })
        .collect();
    let p = plan::plan(
        &inputs,
        &PlanOptions {
            pairwise: None,
            sort_budget: DEFAULT_SORT_BUDGET,
        },
    )
    .unwrap();
    let out = f.base.join("out");
    let r = do_merge(&targets, &out);
    let recs = records(&out.join(".granite/index"));
    assert_eq!(
        p.report.union.materialised_paths,
        r.written + r.linked + r.failed.len() as u64
    );
    assert_eq!(recs.len() as u64, r.written + r.linked);
    let distinct: BTreeMap<[u8; 32], u64> = recs.iter().map(|r| (r.hash, r.size)).collect();
    assert_eq!(p.report.union.distinct_hashes, distinct.len() as u64);
    assert_eq!(
        p.report.union.deduplicated_bytes,
        distinct.values().sum::<u64>()
    );
    assert_eq!(p.report.collisions.count, 0);
    assert_eq!(p.report.shards[3].files, 0);
    let verified = verify::verify(
        &out,
        &VerifyOptions {
            jobs: 2,
            index_dir: None,
            name: None,
            sort_budget: DEFAULT_SORT_BUDGET,
        },
    )
    .unwrap();
    assert!(verified.is_clean(), "{verified:?}");
}

#[test]
fn interrupted_scan_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("shard");
    let files: Vec<(Vec<u8>, Vec<u8>)> = (0..40)
        .map(|i| {
            (
                format!("d{}/f{i}", i % 3).into_bytes(),
                format!("content {i}").into_bytes(),
            )
        })
        .collect();
    let refs: Vec<(&[u8], &[u8])> = files
        .iter()
        .map(|(p, c)| (p.as_slice(), c.as_slice()))
        .collect();
    tree(&root, &refs);
    let full = do_scan(&root, true);
    let recs = records(&root.join(".granite/index"));
    fs::remove_dir_all(root.join(".granite")).unwrap();

    // Half the records journaled, then a torn write at the crash point.
    let mut journal = format!(
        "---\ngranite_journal_version: 0\nproducer: {}\n---\n",
        granite_core::index::PRODUCER
    )
    .into_bytes();
    for r in &recs[..20] {
        journal.extend_from_slice(format!("{}\t{}\t0\t", r.hex(), r.size).as_bytes());
        journal.extend_from_slice(&pathenc::encode(&r.path));
        journal.push(b'\n');
    }
    journal.extend_from_slice(b"0123456789abcdef\t12\t0\ttorn");
    fs::create_dir_all(root.join(".granite")).unwrap();
    fs::write(root.join(".granite/scan.journal"), &journal).unwrap();
    // Journaled files are trusted, not re-read: changing one must not show in the index.
    fs::write(
        root.join(bytes_path(&recs[0].path)),
        b"modified after journal",
    )
    .unwrap();

    let mut notes = Vec::new();
    scan::scan(&root, &scan_opts(true), &mut |m| notes.push(m.to_owned())).unwrap();
    assert!(notes.iter().any(|n| n.contains("resuming")), "{notes:?}");
    assert_eq!(fs::read(root.join(".granite/index")).unwrap(), full);
    assert!(!root.join(".granite/scan.journal").exists());
}

#[test]
fn collisions_block_paths_layout_with_locations() {
    let f = fixture();
    let d = f.base.join("d");
    tree(
        &d,
        &[
            (b"notes.txt", b"notes version D"),
            (b"only-b.bin", b"exclusive to b"),
        ],
    );
    do_scan(&d, true);
    let roots = vec![f.shards[0].clone(), f.shards[2].clone(), d.clone()];
    let out = f.base.join("out");
    let opts = merge_opts();
    let mp = merge::prepare(&roots, &out, &opts).unwrap();
    let groups: Vec<(Vec<u8>, Vec<plan::Claim>)> = mp
        .plan
        .collisions
        .iter()
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(groups.len() as u64, mp.plan.collisions.count());
    let (_, claims) = groups.iter().find(|(p, _)| p == b"notes.txt").unwrap();
    let by_shards: Vec<&[usize]> = claims.iter().map(|c| c.shards.as_slice()).collect();
    assert!(
        by_shards.contains(&&[0, 1][..]) && by_shards.contains(&&[2][..]),
        "{claims:?}"
    );
    assert!(mp.blocked_by_collisions(OutputLayout::Paths));
    assert!(merge::execute(&mp, &out, &opts, &mut |_| {}).is_err());
    assert!(!out.exists());

    let hashed = MergeOptions {
        layout: OutputLayout::Hash,
        ..merge_opts()
    };
    let mp = merge::prepare(&roots, &out, &hashed).unwrap();
    assert!(!mp.blocked_by_collisions(OutputLayout::Hash));
    let r = merge::execute(&mp, &out, &hashed, &mut |_| {}).unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
}

#[test]
fn hash_layout_has_no_collisions() {
    let f = fixture();
    let out = f.base.join("out");
    let opts = MergeOptions {
        layout: OutputLayout::Hash,
        ..merge_opts()
    };
    let mp = merge::prepare(&f.shards, &out, &opts).unwrap();
    let r = merge::execute(&mp, &out, &opts, &mut |_| {}).unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(r.written, mp.plan.report.union.distinct_hashes);
    assert_eq!(
        records(&out.join(".granite/paths")).len() as u64,
        mp.plan.report.union.materialised_paths
    );
    let verified = verify::verify(
        &out,
        &VerifyOptions {
            jobs: 2,
            index_dir: None,
            name: None,
            sort_budget: DEFAULT_SORT_BUDGET,
        },
    )
    .unwrap();
    assert!(verified.is_clean());
}

#[test]
fn path_encoding_fixtures() {
    let cases: &[&[u8]] = &[
        b"\\",
        b"\t",
        b"\n",
        b"\r",
        b"a\\t",
        b"\\\\n",
        b"\x00\x01\x08\x7f",
        b"\x80\xff\xfe",
        b"bad\xff\xfe.bin",
        "caf\u{e9}".as_bytes(),
        "cafe\u{301}".as_bytes(),
    ];
    for c in cases {
        assert_eq!(pathenc::decode(&pathenc::encode(c)).unwrap(), *c);
    }
    assert_ne!(
        pathenc::encode("caf\u{e9}".as_bytes()),
        pathenc::encode("cafe\u{301}".as_bytes())
    );
}

proptest! {
    #[test]
    fn path_encoding_round_trips(p in proptest::collection::vec(any::<u8>().prop_filter("no NUL or /", |&b| b != 0 && b != b'/'), 0..64)) {
        prop_assert_eq!(pathenc::decode(&pathenc::encode(&p)).unwrap(), p);
    }
}
