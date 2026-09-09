use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use granite_core::merge::{self, MergeOptions, MergePlan, OutputLayout};
use granite_core::plan::{self, PlanOptions, PlanReport};
use granite_core::scan::{self, ScanOptions};
use granite_core::verify::{self, VerifyOptions};
use granite_core::{DEFAULT_SORT_BUDGET, Error, default_jobs, hex_encode, index, pathenc, shard};

const EXIT_CODES: &str = "\
Exit codes:
  0  success
  1  usage or argument error
  2  completed with skipped or unreadable files
  3  verification or hash mismatch
  4  index format version or semantic-key refusal";

#[derive(Parser)]
#[command(name = "granite", version, about = "Merge partial copies of a file collection into one deduplicated collection.", after_help = EXIT_CODES)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct IndexLocation {
    /// Write or read the index outside the shard (for read-only media), as <DIR>/<NAME>.index
    #[arg(long, value_name = "DIR")]
    index_dir: Option<PathBuf>,
    /// Name used with --index-dir; defaults to a sanitised form of the shard's basename
    #[arg(long, value_name = "NAME", requires = "index_dir")]
    name: Option<String>,
}

#[derive(Args)]
struct SortBudget {
    /// Memory budget for sorting, in MiB
    #[arg(long, value_name = "MIB", default_value_t = DEFAULT_SORT_BUDGET >> 20)]
    sort_budget: usize,
}

#[derive(Subcommand)]
enum Cmd {
    /// Walk a shard, hash every regular file, and write its index
    Scan {
        root: PathBuf,
        #[command(flatten)]
        loc: IndexLocation,
        /// Ignore hints and re-read every byte
        #[arg(long)]
        force: bool,
        /// Files hashed concurrently
        #[arg(long, value_name = "N", default_value_t = default_jobs())]
        jobs: usize,
        #[command(flatten)]
        sort: SortBudget,
    },
    /// Report deduplicated size and overlap across shards, from indexes alone
    Plan {
        /// Shard roots or index files
        #[arg(required = true)]
        targets: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Force the pairwise overlap matrix even for many shards
        #[arg(long)]
        pairwise: bool,
        #[command(flatten)]
        sort: SortBudget,
    },
    /// Materialise the union of shards into a directory
    Merge {
        /// Shard roots
        #[arg(required = true)]
        targets: Vec<PathBuf>,
        #[arg(long, value_name = "DIR")]
        into: PathBuf,
        #[arg(long, value_enum, default_value_t = LayoutArg::Paths)]
        layout: LayoutArg,
        /// Hardlink from sources on the same device instead of copying
        #[arg(long)]
        link: bool,
        /// Hardlink multiple paths of one hash to each other within the output
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_name = "BOOL")]
        dedupe_links: bool,
        /// Print the plan and any collisions; write nothing
        #[arg(long)]
        dry_run: bool,
        /// Location of the sources' indexes when kept outside the shards
        #[arg(long, value_name = "DIR")]
        index_dir: Option<PathBuf>,
        #[arg(long)]
        pairwise: bool,
        #[command(flatten)]
        sort: SortBudget,
    },
    /// Re-hash a shard and diff it against its index
    Verify {
        root: PathBuf,
        #[arg(long, value_name = "N", default_value_t = default_jobs())]
        jobs: usize,
        #[command(flatten)]
        loc: IndexLocation,
        #[command(flatten)]
        sort: SortBudget,
    },
    /// Print the fingerprint of each shard's index
    Fingerprint {
        /// Shard roots or index files
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Paths,
    Hash,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            let code = e.downcast_ref::<Error>().map_or(1, Error::exit_code);
            ExitCode::from(code as u8)
        }
    }
}

fn log(msg: &str) {
    eprintln!("{msg}");
}

fn run(cli: Cli) -> anyhow::Result<u8> {
    match cli.cmd {
        Cmd::Scan {
            root,
            loc,
            force,
            jobs,
            sort,
        } => {
            let opts = ScanOptions {
                index_dir: loc.index_dir,
                name: loc.name,
                force,
                jobs,
                sort_budget: sort.sort_budget << 20,
            };
            let r = scan::scan(&root, &opts, &mut log)?;
            println!(
                "indexed {} files, {} bytes; skipped {}",
                r.files_indexed, r.bytes_indexed, r.files_skipped
            );
            println!("fingerprint {}", hex_encode(&r.fingerprint));
            if r.files_skipped > 0 {
                println!("see {} for skipped entries", r.layout.skipped.display());
            }
            Ok(if r.files_skipped > 0 { 2 } else { 0 })
        }
        Cmd::Plan {
            targets,
            json,
            pairwise,
            sort,
        } => {
            let inputs = plan::inputs_for_targets(&targets);
            let opts = PlanOptions {
                pairwise: pairwise.then_some(true),
                sort_budget: sort.sort_budget << 20,
            };
            let p = plan::plan(&inputs, &opts)?;
            let mut out = std::io::stdout().lock();
            if json {
                serde_json::to_writer_pretty(&mut out, &p.report)?;
                writeln!(out)?;
            } else {
                print_plan(&mut out, &p.report)?;
            }
            Ok(0)
        }
        Cmd::Merge {
            targets,
            into,
            layout,
            link,
            dedupe_links,
            dry_run,
            index_dir,
            pairwise,
            sort,
        } => {
            let opts = MergeOptions {
                layout: match layout {
                    LayoutArg::Paths => OutputLayout::Paths,
                    LayoutArg::Hash => OutputLayout::Hash,
                },
                link,
                dedupe_links,
                index_dir,
                pairwise: pairwise.then_some(true),
                sort_budget: sort.sort_budget << 20,
            };
            let mp = merge::prepare(&targets, &into, &opts)?;
            let mut out = std::io::stdout().lock();
            if dry_run {
                print_plan(&mut out, &mp.plan.report)?;
                if mp.blocked_by_collisions(opts.layout) {
                    print_collisions(&mut out, &mp)?;
                }
                return Ok(0);
            }
            if mp.blocked_by_collisions(opts.layout) {
                let mut err = std::io::stderr().lock();
                writeln!(
                    err,
                    "error: {} paths are claimed by more than one file; nothing was written",
                    mp.plan.collisions.len()
                )?;
                print_collisions(&mut err, &mp)?;
                return Ok(1);
            }
            let r = merge::execute(&mp, &into, &opts, &mut log)?;
            writeln!(
                out,
                "wrote {} files ({} bytes), linked {}, already present {}, failed {}",
                r.written,
                r.bytes_written,
                r.linked,
                r.existing,
                r.failed.len()
            )?;
            for f in &r.failed {
                writeln!(out, "  failed  {}: {}", pathenc::display(&f.path), f.reason)?;
            }
            writeln!(out, "fingerprint {}", hex_encode(&r.fingerprint))?;
            Ok(if r.failed.is_empty() { 0 } else { 3 })
        }
        Cmd::Verify {
            root,
            jobs,
            loc,
            sort,
        } => {
            let opts = VerifyOptions {
                jobs,
                index_dir: loc.index_dir,
                name: loc.name,
                sort_budget: sort.sort_budget << 20,
            };
            let r = verify::verify(&root, &opts)?;
            let mut out = std::io::stdout().lock();
            if let Some(at) = &r.scanned_at {
                writeln!(out, "index scanned at {at}")?;
            }
            writeln!(out, "matched {}", r.matched)?;
            for p in &r.missing {
                writeln!(out, "  missing   {}", pathenc::display(p))?;
            }
            for p in &r.extra {
                writeln!(out, "  unindexed {}", pathenc::display(p))?;
            }
            for c in &r.changed {
                writeln!(
                    out,
                    "  changed   {}  index {}  disk {}",
                    pathenc::display(&c.path),
                    hex_encode(&c.expected),
                    hex_encode(&c.actual)
                )?;
            }
            for (p, why) in &r.unreadable {
                writeln!(out, "  unreadable {}: {why}", pathenc::display(p))?;
            }
            writeln!(
                out,
                "missing {}, unindexed {}, changed {}, unreadable {}",
                r.missing.len(),
                r.extra.len(),
                r.changed.len(),
                r.unreadable.len()
            )?;
            Ok(if !r.is_clean() {
                3
            } else if !r.unreadable.is_empty() {
                2
            } else {
                0
            })
        }
        Cmd::Fingerprint { targets } => {
            let mut out = std::io::stdout().lock();
            for t in &targets {
                let path = shard::index_for_target(t);
                index::IndexReader::open(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let fp = index::fingerprint(&path)?;
                writeln!(out, "{}  {}", hex_encode(&fp), t.display())?;
            }
            Ok(0)
        }
    }
}

fn print_plan(out: &mut impl Write, r: &PlanReport) -> std::io::Result<()> {
    let w = r
        .shards
        .iter()
        .map(|s| s.label.len())
        .max()
        .unwrap_or(5)
        .max(5);
    writeln!(
        out,
        "{:<w$}  {:>10}  {:>16}  {:>10}  {:>10}  {:>16}",
        "shard", "files", "bytes", "distinct", "only here", "bytes only here"
    )?;
    for s in &r.shards {
        writeln!(
            out,
            "{:<w$}  {:>10}  {:>16}  {:>10}  {:>10}  {:>16}",
            s.label, s.files, s.bytes, s.distinct_hashes, s.unique_hashes, s.unique_bytes
        )?;
    }
    let u = &r.union;
    writeln!(out)?;
    writeln!(out, "union")?;
    writeln!(out, "  distinct hashes     {:>16}", u.distinct_hashes)?;
    writeln!(out, "  deduplicated bytes  {:>16}", u.deduplicated_bytes)?;
    writeln!(out, "  naive bytes         {:>16}", u.naive_bytes)?;
    writeln!(out, "  bytes saved         {:>16}", u.bytes_saved)?;
    match u.ratio {
        Some(ratio) => writeln!(out, "  ratio               {:>16.2}x", ratio)?,
        None => writeln!(out, "  ratio               {:>16}", "n/a")?,
    }
    writeln!(out, "  materialised paths  {:>16}", u.materialised_paths)?;
    writeln!(out)?;
    writeln!(out, "replicas  {:>10}  {:>16}", "hashes", "bytes")?;
    for b in &r.replicas {
        writeln!(out, "  {:<6}  {:>10}  {:>16}", b.copies, b.hashes, b.bytes)?;
    }
    match &r.pairwise {
        Some(pairs) if !pairs.is_empty() => {
            writeln!(out)?;
            writeln!(out, "pairwise overlap  {:>10}  {:>16}", "hashes", "bytes")?;
            for p in pairs {
                writeln!(
                    out,
                    "  [{}] x [{}]  {:>10}  {:>16}",
                    p.a, p.b, p.shared_hashes, p.shared_bytes
                )?;
            }
            writeln!(
                out,
                "  shard indexes refer to the order above, starting at 0"
            )?;
        }
        Some(_) => {}
        None => {
            writeln!(out)?;
            writeln!(
                out,
                "pairwise overlap omitted for {} shards; pass --pairwise to compute it",
                r.shards.len()
            )?;
        }
    }
    writeln!(out)?;
    writeln!(out, "path collisions: {}", r.collisions.count)?;
    if r.collisions.count > 0 {
        writeln!(
            out,
            "  paths claimed by more than one file; merge --layout paths refuses until these are renamed"
        )?;
        for c in &r.collisions.sample {
            writeln!(out, "  {}", c.path)?;
            for claim in &c.claims {
                for &i in &claim.shards {
                    writeln!(
                        out,
                        "      {}  {}/{}",
                        claim.hash, r.shards[i].label, c.path
                    )?;
                }
            }
        }
        if r.collisions.count as usize > r.collisions.sample.len() {
            writeln!(
                out,
                "  ... and {} more",
                r.collisions.count as usize - r.collisions.sample.len()
            )?;
        }
    }
    Ok(())
}

/// The complete resolution list: every colliding path, and where each
/// claimant lives so the user can rename one and rescan that shard.
fn print_collisions(out: &mut impl Write, mp: &MergePlan) -> std::io::Result<()> {
    let labels = &mp.plan.report.shards;
    writeln!(out)?;
    writeln!(
        out,
        "resolve by renaming all but one claimant of each path in its shard, then `granite scan` that shard:"
    )?;
    for (path, claims) in &mp.plan.collisions {
        let shown = pathenc::display(path);
        writeln!(out, "  {shown}")?;
        for claim in claims {
            for &i in &claim.shards {
                writeln!(
                    out,
                    "      {}  {}/{shown}",
                    hex_encode(&claim.hash),
                    labels[i].label
                )?;
            }
        }
    }
    Ok(())
}
