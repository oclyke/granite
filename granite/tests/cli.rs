use std::fs;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;

fn granite() -> Command {
    Command::cargo_bin("granite").unwrap()
}

fn shard(root: &Path, files: &[(&str, &str)]) {
    for (p, c) in files {
        let path = root.join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, c).unwrap();
    }
    granite().arg("scan").arg(root).assert().success();
}

#[test]
fn index_records_pass_c_sort() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    shard(
        &root,
        &[
            ("b/z\\x", "1"),
            ("a", "2"),
            ("b/z", "3"),
            ("caf\u{e9}", "4"),
            ("same", "2"),
        ],
    );
    let index = fs::read(root.join(".granite/index")).unwrap();
    let body_at = index
        .windows(5)
        .enumerate()
        .skip(1)
        .find(|(_, w)| *w == b"\n---\n")
        .map(|(i, _)| i + 5)
        .unwrap();
    let body = dir.path().join("body");
    fs::write(&body, &index[body_at..]).unwrap();
    let status = Command::new("sort")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(&body)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn fingerprint_matches_meta_and_is_path_independent() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("first/location");
    let b = dir.path().join("second");
    shard(&a, &[("x/y", "hello"), ("z", "")]);
    shard(&b, &[("x/y", "hello"), ("z", "")]);
    let out = granite()
        .arg("fingerprint")
        .arg(&a)
        .arg(b.join(".granite/index"))
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let fps: Vec<&str> = text
        .lines()
        .map(|l| l.split("  ").next().unwrap())
        .collect();
    assert_eq!(fps[0], fps[1]);
    let meta = fs::read_to_string(a.join(".granite/scan.meta")).unwrap();
    assert!(meta.contains(&format!("index_fingerprint: {}", fps[0])));
}

#[test]
fn version_gate_refuses_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    shard(&a, &[("f", "1")]);
    shard(&b, &[("g", "2")]);
    let index = a.join(".granite/index");
    let good = fs::read_to_string(&index).unwrap();
    let variants = [
        good.replacen("producer: granite ", "producer: granite 0.0.0-other ", 1),
        good.replacen("granite_index_version: 0", "granite_index_version: 7", 1),
        good.replacen("requires: []", "requires: [tombstones]", 1),
        good.replacen("hash: blake3-256", "hash: sha256-256", 1),
    ];
    for bad in &variants {
        assert_ne!(bad, &good);
        fs::write(&index, bad).unwrap();
        granite().arg("plan").arg(&a).arg(&b).assert().code(4);
        granite().arg("fingerprint").arg(&a).assert().code(4);
        granite().arg("verify").arg(&a).assert().code(4);
        granite()
            .arg("merge")
            .arg(&a)
            .arg(&b)
            .arg("--into")
            .arg(dir.path().join("out"))
            .assert()
            .code(4);
    }
    fs::write(&index, &variants[0]).unwrap();
    let out = granite().arg("plan").arg(&a).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("re-run `granite scan --force`"));
}

#[test]
fn exit_codes_for_skips_and_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    shard(&root, &[("f", "1")]);
    std::os::unix::fs::symlink("f", root.join("link")).unwrap();
    granite().arg("scan").arg(&root).assert().code(2);
    granite().arg("verify").arg(&root).assert().code(0);
    fs::write(root.join("f"), "2").unwrap();
    granite().arg("verify").arg(&root).assert().code(3);
    granite()
        .arg("plan")
        .arg(dir.path().join("nope"))
        .assert()
        .code(1);
}

#[test]
fn plan_json_and_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    shard(&a, &[("same.txt", "A"), ("shared", "s")]);
    shard(&b, &[("same.txt", "B"), ("shared", "s")]);
    let out = granite()
        .arg("plan")
        .arg(&a)
        .arg(&b)
        .arg("--json")
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["collisions"]["count"], 1);
    assert_eq!(v["union"]["materialised_paths"], 3);
    assert_eq!(v["replicas"][1]["hashes"], 1);
    let into = dir.path().join("out");
    let dry = granite()
        .arg("merge")
        .arg(&a)
        .arg(&b)
        .arg("--into")
        .arg(&into)
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(dry.status.success());
    let text = String::from_utf8_lossy(&dry.stdout);
    assert!(
        text.contains(&format!("{}/same.txt", a.display()))
            && text.contains(&format!("{}/same.txt", b.display()))
    );
    let refused = granite()
        .arg("merge")
        .arg(&a)
        .arg(&b)
        .arg("--into")
        .arg(&into)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("same.txt"));
    assert!(!into.exists());
}

#[test]
fn external_index_dir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("drive one");
    let idx = dir.path().join("indexes");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("f"), "1").unwrap();
    granite()
        .arg("scan")
        .arg(&root)
        .arg("--index-dir")
        .arg(&idx)
        .assert()
        .success();
    assert!(idx.join("drive_one.index").exists());
    assert!(!root.join(".granite").exists());
    granite()
        .arg("verify")
        .arg(&root)
        .arg("--index-dir")
        .arg(&idx)
        .assert()
        .success();
    granite()
        .arg("merge")
        .arg(&root)
        .arg("--into")
        .arg(dir.path().join("out"))
        .arg("--index-dir")
        .arg(&idx)
        .assert()
        .success();
}
