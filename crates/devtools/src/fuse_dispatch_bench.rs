// SPDX-License-Identifier: Apache-2.0
//! `fuse-dispatch-bench` — compare the three dispatch-shape options
//! for parallel-agent Rust workloads on Linux:
//!
//! 1. `git` — `git worktree add` per agent (the current HeddleCo
//!    orchestrator default).
//! 2. `solid` — `heddle start --workspace solid` per agent (full
//!    file copies; the "heavy mode" the issue refers to).
//! 3. `virt` — `heddle start --workspace virtualized --daemon`
//!    per agent (FUSE-backed CoW projection, mount owned by the
//!    long-lived `heddled` daemon).
//!
//! Issue: HeddleCo/heddle#164. Mac numbers exist already; this is
//! the Linux pass that gates an orchestrator migration from
//! `git worktree` dispatch to `heddle thread` dispatch.
//!
//! ## Invocation
//!
//! ```text
//! cargo run --release -p heddle-devtools -- fuse-dispatch-bench \
//!     --workload <path-to-cargo-workspace-source> \
//!     --heddle-bin <path-to-heddle> \
//!     --parallel 3 \
//!     --modes git,solid,virt \
//!     --stress-secs 0 \
//!     --json out.json \
//!     --md out.md
//! ```
//!
//! The harness is intentionally side-effect-only: it shells out to
//! `git`, the user-provided `heddle` binary, and `cargo`. It does
//! not link against heddle's own crates. That keeps the bench
//! decoupled from the in-repo CLI version-skew risks (you can
//! bench an old or new heddle by pointing `--heddle-bin` at it).
//!
//! ## Measurement notes (for downstream readers)
//!
//! * **Cold builds use a per-workdir `CARGO_TARGET_DIR`.** Mode 1 of
//!   the issue's wording calls out the orchestrator's shared-target
//!   trick; in the bench we deliberately don't share, to keep the
//!   inter-mode comparison fair. The "what does shared-target save"
//!   number is orthogonal and not what this issue is asking for.
//! * **`time` uses wall-clock + process-wide CPU.** We don't try to
//!   measure FUSE worker CPU separately — it's part of the user-
//!   observed cost.
//! * **Disk usage is `du -sb` (bytes, no apparent-size).** That counts
//!   block-allocated bytes, so reflink-shared blocks won't be
//!   double-counted on btrfs/xfs+reflink/bcachefs. On ext4
//!   (no reflink) `solid` and `materialized` collapse to the same
//!   shape.
//! * **Parallel runs use OS threads to spawn child processes**, not a
//!   shell `&` loop, so we get clean per-child timing.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};

// ---------- CLI ----------

#[derive(Debug)]
struct Args {
    workload: PathBuf,
    heddle_bin: PathBuf,
    out_dir: PathBuf,
    parallel: usize,
    modes: Vec<Mode>,
    stress_secs: u64,
    json_out: Option<PathBuf>,
    md_out: Option<PathBuf>,
    keep_workdirs: bool,
    /// Per-invocation uniqueness token, used to disambiguate names
    /// (git branches, heddle thread names, default out-dir suffix) so
    /// that two runs in the same shell — or two runs in the same wall
    /// second — don't collide. Format: `<nanos>-<pid>`.
    run_token: String,
}

fn make_run_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{nanos:x}-{pid}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Git,
    Solid,
    Virt,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Git => "git",
            Mode::Solid => "solid",
            Mode::Virt => "virt",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "git" => Mode::Git,
            "solid" | "heavy" => Mode::Solid,
            "virt" | "virtualized" | "fuse" => Mode::Virt,
            _ => bail!("unknown mode '{s}' (expected: git, solid, virt)"),
        })
    }
}

fn parse_args(raw: Vec<String>) -> Result<Args> {
    let mut workload: Option<PathBuf> = None;
    let mut heddle_bin: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut parallel: usize = 3;
    let mut modes_s: Option<String> = None;
    let mut stress_secs: u64 = 0;
    let mut json_out: Option<PathBuf> = None;
    let mut md_out: Option<PathBuf> = None;
    let mut keep_workdirs = false;

    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        let take = |it: &mut std::vec::IntoIter<String>, name: &str| -> Result<String> {
            it.next().with_context(|| format!("{name} expects a value"))
        };
        match a.as_str() {
            "--workload" => workload = Some(PathBuf::from(take(&mut it, "--workload")?)),
            "--heddle-bin" => heddle_bin = Some(PathBuf::from(take(&mut it, "--heddle-bin")?)),
            "--out-dir" => out_dir = Some(PathBuf::from(take(&mut it, "--out-dir")?)),
            "--parallel" => parallel = take(&mut it, "--parallel")?.parse()?,
            "--modes" => modes_s = Some(take(&mut it, "--modes")?),
            "--stress-secs" => stress_secs = take(&mut it, "--stress-secs")?.parse()?,
            "--json" => json_out = Some(PathBuf::from(take(&mut it, "--json")?)),
            "--md" => md_out = Some(PathBuf::from(take(&mut it, "--md")?)),
            "--keep-workdirs" => keep_workdirs = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unknown arg '{other}' (see --help)"),
        }
    }

    let workload = workload.context("--workload is required")?.canonicalize()?;
    let heddle_bin = heddle_bin
        .context("--heddle-bin is required")?
        .canonicalize()?;
    let run_token = make_run_token();
    let out_dir = match out_dir {
        Some(p) => {
            fs::create_dir_all(&p)?;
            p.canonicalize()?
        }
        None => {
            let p = std::env::temp_dir().join(format!("fuse-dispatch-bench-{run_token}"));
            fs::create_dir_all(&p)?;
            p
        }
    };
    let modes = modes_s
        .unwrap_or_else(|| "git,solid,virt".to_string())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Mode::parse)
        .collect::<Result<Vec<_>>>()?;
    if modes.is_empty() {
        bail!("--modes resolved to empty");
    }
    if parallel == 0 {
        bail!("--parallel must be >= 1");
    }

    Ok(Args {
        workload,
        heddle_bin,
        out_dir,
        parallel,
        modes,
        stress_secs,
        json_out,
        md_out,
        keep_workdirs,
        run_token,
    })
}

fn print_help() {
    eprintln!(
        "fuse-dispatch-bench — compare git-worktree vs heddle-thread dispatch on Linux

Required:
  --workload <PATH>        path to a cargo workspace to use as the test load
  --heddle-bin <PATH>      path to a heddle binary built with --features mount
                           (the user-facing flag for the FUSE mount backend)

Optional:
  --out-dir <PATH>         where to put generated workdirs (default: $TMPDIR/...)
  --parallel <N>           parallel agents per mode (default: 3)
  --modes <LIST>           comma-separated subset of: git,solid,virt (default: all)
  --stress-secs <N>        run a stress-test loop for N seconds after the
                           main matrix (default: 0 = skip)
  --json <PATH>            write machine-readable results to PATH
  --md <PATH>              write markdown summary to PATH
  --keep-workdirs          leave generated workdirs on disk for inspection

See crates/devtools/src/fuse_dispatch_bench.rs for measurement notes.",
    );
}

// ---------- Result shape ----------

#[derive(Debug, Default, Clone)]
struct ParallelTiming {
    /// Per-child wall-clock seconds.
    per_child_secs: Vec<f64>,
    /// Wall-clock seconds from first-child-start to last-child-finish.
    aggregate_secs: f64,
    /// Whether all children exited 0.
    all_ok: bool,
    /// First-failure message, if any.
    error: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct ModeResult {
    mode: String,
    create: ParallelTiming,
    cold_check: ParallelTiming,
    incremental_check: ParallelTiming,
    cold_release: ParallelTiming,
    /// Per-child bytes used by the workdir post-create, pre-build.
    disk_post_create: Vec<u64>,
    /// Per-child bytes used by the workdir + cargo target post-build.
    disk_post_build: Vec<u64>,
    notes: Vec<String>,
}

#[derive(Debug, Default, Clone)]
struct StressOutcome {
    duration_secs: u64,
    parallel: usize,
    mode: String,
    iters_per_child: Vec<u64>,
    /// First-iter wall-clock per child, in seconds (warm-up baseline).
    first_iter_secs: Vec<f64>,
    /// Last-iter wall-clock per child, in seconds (degradation signal).
    last_iter_secs: Vec<f64>,
    failures: Vec<String>,
    fuse_dmesg_excerpts: Vec<String>,
}

// ---------- Entry point ----------

pub fn run(raw: Vec<String>) -> Result<()> {
    let args = parse_args(raw)?;
    eprintln!("== fuse-dispatch-bench ==");
    eprintln!("workload    : {}", args.workload.display());
    eprintln!("heddle bin  : {}", args.heddle_bin.display());
    eprintln!("out dir     : {}", args.out_dir.display());
    eprintln!("parallel    : {}", args.parallel);
    eprintln!(
        "modes       : {}",
        args.modes
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    eprintln!("stress secs : {}", args.stress_secs);
    eprintln!();

    sanity_workload(&args.workload)?;
    unmount_stale_fuse_under(&args.out_dir);
    let env = Environment::capture();
    eprintln!("host        : {}", env.summary_line());
    eprintln!();

    // Sources: we need (a) a git repo of the workload (for `git` mode),
    // and (b) a heddle repo of the workload (for solid/virt modes).
    let sources = prepare_sources(&args)?;
    eprintln!("sources prepared:");
    eprintln!("  git source    : {}", sources.git_source.display());
    if let Some(p) = &sources.heddle_source {
        eprintln!("  heddle source : {}", p.display());
    }
    eprintln!();

    let mut results: Vec<ModeResult> = Vec::new();
    for mode in &args.modes {
        eprintln!("--- mode: {} ---", mode.as_str());
        let res = run_matrix(*mode, &args, &sources)?;
        eprintln!("done: {}", mode.as_str());
        eprintln!();
        results.push(res);
    }

    let mut stress: Option<StressOutcome> = None;
    if args.stress_secs > 0 {
        let mode = args
            .modes
            .iter()
            .copied()
            .find(|m| *m == Mode::Virt)
            .unwrap_or_else(|| args.modes[0]);
        eprintln!(
            "--- stress test ({} secs, mode={}) ---",
            args.stress_secs,
            mode.as_str()
        );
        stress = Some(run_stress(mode, &args, &sources)?);
        eprintln!("stress done");
        eprintln!();
    }

    if let Some(p) = &args.json_out {
        write_json(p, &env, &args, &results, &stress)?;
        eprintln!("json written: {}", p.display());
    }
    if let Some(p) = &args.md_out {
        write_markdown(p, &env, &args, &results, &stress)?;
        eprintln!("md written  : {}", p.display());
    }

    print_summary(&env, &args, &results, &stress);

    if !args.keep_workdirs {
        // Best-effort cleanup of mode workdirs (sources stay so a re-run
        // doesn't re-import). Failures here are logged not fatal.
        // `stress-<mode>` trees are bench-owned too and carry their own
        // `.cargo-target` directories, which are sizable — drop those
        // in the same pass.
        for mode in &args.modes {
            let _ = fs::remove_dir_all(args.out_dir.join(format!("work-{}", mode.as_str())));
            let _ = fs::remove_dir_all(args.out_dir.join(format!("stress-{}", mode.as_str())));
        }
    }
    Ok(())
}

// ---------- Sanity / environment ----------

/// Unmount any leftover FUSE mounts under `root` from a prior aborted
/// run. Best-effort; failures (no mounts, or unprivileged caller) are
/// silently ignored. Without this, a re-run trips over `rm -rf` of a
/// live mount point and aborts before measurement starts.
///
/// Scoping is intentionally narrow: a mount only qualifies if **all**
/// of these hold, so a user pointing `--out-dir` at a broad subtree
/// (`/tmp`, `$HOME`) can't make us unmount an unrelated sshfs/ntfs-3g
/// or another in-flight heddle workflow.
///   1. Filesystem type starts with `fuse` (excludes sshfs-as-non-fuse
///      edge cases and any non-FUSE bind mounts).
///   2. Mount source identifies as heddle (`heddle-mount` is what the
///      daemon registers; we also accept any source containing
///      `heddle` to remain forward-compatible).
///   3. Mountpoint path contains a `bench-` component — the bench is
///      the only producer that names threads `bench-<mode>-<idx>-…`,
///      so this filters out concurrent non-bench heddle threads.
///   4. Mountpoint is under the canonical out-dir.
fn unmount_stale_fuse_under(root: &Path) {
    let out = match Command::new("mount").output() {
        Ok(o) => o,
        Err(_) => return,
    };
    let body = String::from_utf8_lossy(&out.stdout);
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for line in body.lines() {
        // mount lines look like: "<src> on <mp> type <fstype> (<opts>)"
        let Some((src, rest)) = line.split_once(" on ") else {
            continue;
        };
        let Some((mp, rest)) = rest.split_once(" type ") else {
            continue;
        };
        let fstype = rest.split_whitespace().next().unwrap_or("");
        if !fstype.starts_with("fuse") {
            continue;
        }
        if !src.contains("heddle") {
            continue;
        }
        let mp_path = Path::new(mp);
        let mp_canon = mp_path
            .canonicalize()
            .unwrap_or_else(|_| mp_path.to_path_buf());
        if !mp_canon.starts_with(&canonical_root) {
            continue;
        }
        // Require a `bench-` component anywhere in the path. This is the
        // bench-owned tag — heddle thread names produced by this tool
        // always start with `bench-<mode>-`, so the mount path contains
        // a `bench-…` component. Without this filter, an unrelated
        // heddle thread mounted under a shared out-dir would be torn
        // down here.
        if !mp_canon.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| s.starts_with("bench-"))
        }) {
            continue;
        }
        let _ = Command::new("fusermount3").args(["-u", mp]).status();
        let _ = Command::new("fusermount").args(["-u", mp]).status();
    }
}

fn sanity_workload(workload: &Path) -> Result<()> {
    let cargo_toml = workload.join("Cargo.toml");
    if !cargo_toml.is_file() {
        bail!(
            "workload {} is not a cargo workspace (no Cargo.toml)",
            workload.display()
        );
    }
    Ok(())
}

#[derive(Debug, Default)]
struct Environment {
    kernel: String,
    distro: String,
    cpu_model: String,
    cpu_threads: usize,
    mem_total_kb: u64,
    fs_type: String,
    cargo_version: String,
}

impl Environment {
    fn capture() -> Self {
        let kernel = read_first_line_cmd("uname", &["-r"]).unwrap_or_default();
        let distro = read_first_line(Path::new("/etc/os-release"))
            .unwrap_or_default()
            .trim_start_matches("PRETTY_NAME=")
            .trim_matches('"')
            .to_string();
        let mut cpu_model = String::new();
        if let Ok(s) = fs::read_to_string("/proc/cpuinfo") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("model name") {
                    cpu_model = rest
                        .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
                        .to_string();
                    break;
                }
            }
        }
        let cpu_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
        let mut mem_total_kb: u64 = 0;
        if let Ok(s) = fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    mem_total_kb = rest.trim().trim_end_matches(" kB").parse().unwrap_or(0);
                    break;
                }
            }
        }
        let fs_type = read_first_line_cmd("stat", &["-f", "-c", "%T", "/"]).unwrap_or_default();
        let cargo_version = read_first_line_cmd("cargo", &["--version"]).unwrap_or_default();
        Environment {
            kernel,
            distro,
            cpu_model,
            cpu_threads,
            mem_total_kb,
            fs_type,
            cargo_version,
        }
    }
    fn summary_line(&self) -> String {
        format!(
            "{} | kernel {} | {} threads | {:.1} GiB RAM | rootfs={}",
            self.distro,
            self.kernel,
            self.cpu_threads,
            self.mem_total_kb as f64 / 1024.0 / 1024.0,
            self.fs_type,
        )
    }
}

fn read_first_line(p: &Path) -> Result<String> {
    let s = fs::read_to_string(p)?;
    for line in s.lines() {
        if line.starts_with("PRETTY_NAME=") {
            return Ok(line.to_string());
        }
    }
    Ok(s.lines().next().unwrap_or("").to_string())
}

fn read_first_line_cmd(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog).args(args).output()?;
    if !out.status.success() {
        return Err(anyhow!("{prog} {args:?} exit={:?}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string())
}

// ---------- Source preparation ----------

struct Sources {
    /// A git repo whose `main` branch holds a snapshot of the workload.
    git_source: PathBuf,
    /// A heddle repo (with the workload imported) for solid/virt modes.
    /// `None` if no solid/virt mode is requested.
    heddle_source: Option<PathBuf>,
}

fn prepare_sources(args: &Args) -> Result<Sources> {
    let snapshot = args.out_dir.join("snapshot");
    let manifest_path = snapshot.join(".manifest");
    let want_manifest = workload_manifest(&args.workload)?;
    let manifest_match = manifest_path
        .is_file()
        .then(|| fs::read_to_string(&manifest_path).ok())
        .flatten()
        .is_some_and(|s| s.trim() == want_manifest);
    // Whenever the workload fingerprint changes (or no fingerprint
    // exists yet), blow away the derived stack — snapshot, git-source,
    // heddle-source. Without this, a second run with `--out-dir`
    // reused after the user edited the workload would silently
    // benchmark the *old* workload contents.
    if !manifest_match {
        let _ = fs::remove_dir_all(&snapshot);
        let _ = fs::remove_dir_all(args.out_dir.join("git-source"));
        let _ = fs::remove_dir_all(args.out_dir.join("heddle-source"));
    }
    if !snapshot.exists() {
        copy_tree(&args.workload, &snapshot)?;
        // Strip out any existing .git so we own the history shape.
        let _ = fs::remove_dir_all(snapshot.join(".git"));
        let _ = fs::remove_dir_all(snapshot.join(".heddle"));
        fs::write(&manifest_path, &want_manifest)?;
    }

    let git_source = args.out_dir.join("git-source");
    if !git_source.exists() {
        copy_tree(&snapshot, &git_source)?;
        run_in(&git_source, "git", &["init", "-q", "-b", "main"])?;
        run_in(&git_source, "git", &["config", "user.email", "bench@local"])?;
        run_in(&git_source, "git", &["config", "user.name", "bench"])?;
        run_in(&git_source, "git", &["add", "-A"])?;
        run_in(
            &git_source,
            "git",
            &["commit", "-q", "-m", "bench: initial workload snapshot"],
        )?;
    }

    let need_heddle = args
        .modes
        .iter()
        .any(|m| matches!(m, Mode::Solid | Mode::Virt));
    let heddle_source = if need_heddle {
        let hs = args.out_dir.join("heddle-source");
        if !hs.exists() {
            // `heddle import git` adopts a git branch as a heddle
            // lane. We import from git_source.
            sh(
                &args.heddle_bin,
                &[
                    "init",
                    "--no-harness-install",
                    "--principal-name",
                    "bench",
                    "--principal-email",
                    "bench@local",
                    hs.to_str().unwrap(),
                ],
            )?;
            // Populate from git_source by copying contents and capturing.
            // (We use the simpler `init+copy+capture` shape rather than
            // bridge-git-import because the bridge expects a git remote
            // wiring we don't need for a single-state workload.)
            copy_tree_contents(&git_source, &hs)?;
            let _ = fs::remove_dir_all(hs.join(".git"));
            // Capture the initial state so threads have something to start from.
            let out = Command::new(&args.heddle_bin)
                .arg("--repo")
                .arg(&hs)
                .args(["capture", "--intent", "bench: import workload"])
                .output()?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                bail!("heddle capture failed: {}", stderr);
            }
        }
        Some(hs)
    } else {
        None
    };

    Ok(Sources {
        git_source,
        heddle_source,
    })
}

/// Fingerprint of the workload tree, stored in `snapshot/.manifest` so
/// a reused `--out-dir` can detect a stale snapshot and re-copy. The
/// digest mixes each file's relative path, byte length, and mtime
/// (nanos); identical-content reruns hash to the same value, and any
/// edit to the workload flips it. Skip noisy dirs that shouldn't be
/// part of the input (`target/`, `.git/`, etc.).
fn workload_manifest(root: &Path) -> Result<String> {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    let mut entries: Vec<(String, u64, i128)> = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                "target" | ".cargo-target" | ".git" | ".heddle" | "node_modules"
            )
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let len = md.len();
        let mtime_nanos: i128 = md
            .modified()
            .ok()
            .and_then(|t| {
                t.duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i128)
                    .ok()
            })
            .unwrap_or(0);
        entries.push((rel, len, mtime_nanos));
    }
    entries.sort();
    let mut h = DefaultHasher::new();
    entries.hash(&mut h);
    Ok(format!("v1 {:016x}", h.finish()))
}

// ---------- The matrix ----------

fn run_matrix(mode: Mode, args: &Args, sources: &Sources) -> Result<ModeResult> {
    let mut res = ModeResult {
        mode: mode.as_str().to_string(),
        ..Default::default()
    };

    // 1. Create N parallel workdirs.
    let parent = args.out_dir.join(format!("work-{}", mode.as_str()));
    // Always recreate from scratch — measurement requires cold state.
    let _ = fs::remove_dir_all(&parent);
    fs::create_dir_all(&parent)?;

    let desired: Vec<PathBuf> = (0..args.parallel)
        .map(|i| parent.join(format!("agent-{i}")))
        .collect();

    // Result slots, populated as create_workdir completes.
    let actual_slots: Vec<std::sync::Mutex<Option<PathBuf>>> = (0..args.parallel)
        .map(|_| std::sync::Mutex::new(None))
        .collect();

    eprintln!("  step: create {} workdirs", args.parallel);
    res.create = run_parallel(args.parallel, |i| -> Result<()> {
        let actual = create_workdir(mode, args, sources, &desired[i], i)?;
        *actual_slots[i].lock().unwrap() = Some(actual);
        Ok(())
    });
    // Drain slots regardless of success: on partial failure we still
    // need to know which children created something so we can tear those
    // down rather than leaking mounts/worktrees/branches.
    let created: Vec<(usize, PathBuf)> = actual_slots
        .into_iter()
        .enumerate()
        .filter_map(|(i, s)| s.lock().unwrap().take().map(|p| (i, p)))
        .collect();
    if !res.create.all_ok {
        res.notes
            .push(format!("create failed: {:?}", res.create.error));
        let partial_workdirs: Vec<PathBuf> = created.iter().map(|(_, p)| p.clone()).collect();
        let partial_branches: Vec<Option<String>> = created
            .iter()
            .map(|(i, _)| branch_name_for(mode, args, *i))
            .collect();
        teardown_workdirs(mode, sources, &partial_workdirs, &partial_branches);
        return Ok(res);
    }
    // All N children succeeded — `created` has length args.parallel in
    // index order.
    let workdirs: Vec<PathBuf> = created.iter().map(|(_, p)| p.clone()).collect();
    let branches: Vec<Option<String>> = (0..args.parallel)
        .map(|i| branch_name_for(mode, args, i))
        .collect();

    // 2. Disk usage post-create.
    res.disk_post_create = workdirs.iter().map(|w| dir_size_bytes(w)).collect();

    // 3. Cold cargo check (per-workdir CARGO_TARGET_DIR).
    eprintln!("  step: cold cargo check");
    res.cold_check = run_parallel(args.parallel, |i| -> Result<()> {
        run_cargo(&workdirs[i], &["check", "--workspace"])
    });

    // 4. Incremental cargo check (after touching one file).
    if res.cold_check.all_ok {
        for w in &workdirs {
            touch_one_file(w)?;
        }
        eprintln!("  step: incremental cargo check");
        res.incremental_check = run_parallel(args.parallel, |i| -> Result<()> {
            run_cargo(&workdirs[i], &["check", "--workspace"])
        });
    }

    // 5. Cold cargo build --release. To re-cold the build, clear the
    //    per-workdir target dir first.
    if res.cold_check.all_ok {
        for w in &workdirs {
            let _ = fs::remove_dir_all(w.join(".cargo-target"));
        }
        eprintln!("  step: cold cargo build --release");
        res.cold_release = run_parallel(args.parallel, |i| -> Result<()> {
            run_cargo(&workdirs[i], &["build", "--release", "--workspace"])
        });
    }

    // 6. Disk usage post-build.
    res.disk_post_build = workdirs.iter().map(|w| dir_size_bytes(w)).collect();

    // 7. Teardown.
    teardown_workdirs(mode, sources, &workdirs, &branches);

    Ok(res)
}

/// Reconstruct the branch name used by `create_workdir` for a given
/// (mode, idx) so teardown can delete the ref. Returns `None` for
/// non-git modes.
fn branch_name_for(mode: Mode, args: &Args, idx: usize) -> Option<String> {
    match mode {
        Mode::Git => Some(format!("bench-{idx}-{}", args.run_token)),
        Mode::Solid | Mode::Virt => None,
    }
}

/// Create a workdir for `mode` and return the *actual* on-disk path
/// where cargo should be invoked. For git and solid modes that matches
/// `desired_path`; for virtualized mode the heddle CLI ignores `--path`
/// and mounts under `<repo>/.heddle/threads/<thread>` —
/// we parse the JSON `path` field from stdout to discover it.
fn create_workdir(
    mode: Mode,
    args: &Args,
    sources: &Sources,
    desired_path: &Path,
    idx: usize,
) -> Result<PathBuf> {
    match mode {
        Mode::Git => {
            // Suffix with the per-run token so repeated invocations
            // against the same `--out-dir` (and thus the same reused
            // `git-source` repo) don't collide on an existing
            // `bench-<idx>` branch ref.
            sh(
                "git",
                &[
                    "-C",
                    sources.git_source.to_str().unwrap(),
                    "worktree",
                    "add",
                    desired_path.to_str().unwrap(),
                    "-b",
                    &format!("bench-{idx}-{}", args.run_token),
                    "main",
                ],
            )?;
            Ok(desired_path.to_path_buf())
        }
        Mode::Solid | Mode::Virt => {
            let hs = sources
                .heddle_source
                .as_ref()
                .ok_or_else(|| anyhow!("{} mode needs a heddle source", mode.as_str()))?;
            let workspace = match mode {
                Mode::Solid => "solid",
                Mode::Virt => "virtualized",
                Mode::Git => unreachable!(),
            };
            let mut cmd = Command::new(&args.heddle_bin);
            cmd.args(["--repo", hs.to_str().unwrap()])
                .args([
                    "start",
                    // Suffix with the per-run token so a rerun of the
                    // bench against the same heddle source doesn't
                    // collide with the previous run's still-registered
                    // thread name (`bench-virt-0`, etc.).
                    &format!("bench-{}-{idx}-{}", mode.as_str(), args.run_token),
                    "--workspace",
                    workspace,
                    "--path",
                    desired_path.to_str().unwrap(),
                    "--automated",
                    "--output",
                    "json",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if mode == Mode::Virt {
                // Default `--daemon` hands the FUSE mount off to the
                // long-lived `heddled` daemon, which keeps it alive
                // after this `heddle start` exits. We explicitly set
                // it so a user-config default of `no-daemon` doesn't
                // break the bench. Teardown unmounts via fusermount.
                cmd.arg("--daemon");
            }
            let out = cmd.output()?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                bail!(
                    "heddle start ({}): exit={:?}\n{}",
                    workspace,
                    out.status,
                    stderr
                );
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Real JSON decode — earlier versions sliced on a raw `"`,
            // which broke as soon as the daemon emitted a path
            // containing an escaped quote (or any future field that
            // happened to contain the substring `"path":"`).
            let actual = extract_thread_path(&stdout).with_context(|| {
                format!("could not parse 'path' from heddle start stdout: {stdout}")
            })?;
            Ok(PathBuf::from(actual))
        }
    }
}

/// Decode the workdir / mount path out of `heddle start --output json`
/// stdout. The daemon emits a JSON object with a top-level `path` field
/// and a nested `thread.path` carrying the same value; we prefer the
/// top-level one but fall back to the nested form for older heddles.
fn extract_thread_path(stdout: &str) -> Result<String> {
    // Strip any leading non-JSON noise (a stray banner line, etc.) by
    // locating the first `{`.
    let json_start = stdout
        .find('{')
        .ok_or_else(|| anyhow!("no JSON object found"))?;
    let v: serde_json::Value = serde_json::from_str(&stdout[json_start..])
        .map_err(|e| anyhow!("invalid JSON from heddle start: {e}"))?;
    if let Some(p) = v.get("path").and_then(|x| x.as_str()) {
        return Ok(p.to_string());
    }
    if let Some(p) = v
        .get("thread")
        .and_then(|t| t.get("path"))
        .and_then(|x| x.as_str())
    {
        return Ok(p.to_string());
    }
    Err(anyhow!("no 'path' field in heddle start JSON"))
}

fn teardown_workdirs(
    mode: Mode,
    sources: &Sources,
    workdirs: &[PathBuf],
    branches: &[Option<String>],
) {
    for (i, w) in workdirs.iter().enumerate() {
        match mode {
            Mode::Git => {
                // git worktree remove handles the metadata; even if it
                // fails (e.g. dirty), the parent dir gets blown away
                // by the caller when --keep-workdirs is off. Must run
                // with `-C <source>` so git can find the metadata.
                let _ = Command::new("git")
                    .args(["-C", sources.git_source.to_str().unwrap()])
                    .args(["worktree", "remove", "-f", w.to_str().unwrap()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                // Delete the per-run branch ref so reruns against the
                // same `--out-dir` (and therefore same reused git
                // source) don't accumulate `bench-<idx>-<token>`
                // branches forever. `worktree remove` leaves the branch
                // intact by design.
                if let Some(Some(br)) = branches.get(i) {
                    let _ = Command::new("git")
                        .args(["-C", sources.git_source.to_str().unwrap()])
                        .args(["branch", "-D", br])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
            Mode::Virt => {
                // In-process mount: best-effort fusermount.
                let _ = Command::new("fusermount3")
                    .args(["-u", w.to_str().unwrap()])
                    .status();
                let _ = Command::new("fusermount")
                    .args(["-u", w.to_str().unwrap()])
                    .status();
            }
            Mode::Solid => { /* nothing extra */ }
        }
    }
}

// ---------- Stress test ----------

fn run_stress(mode: Mode, args: &Args, sources: &Sources) -> Result<StressOutcome> {
    let parent = args.out_dir.join(format!("stress-{}", mode.as_str()));
    let _ = fs::remove_dir_all(&parent);
    fs::create_dir_all(&parent)?;

    let desired: Vec<PathBuf> = (0..args.parallel)
        .map(|i| parent.join(format!("agent-{i}")))
        .collect();
    let mut workdirs: Vec<PathBuf> = Vec::with_capacity(args.parallel);
    let mut branches: Vec<Option<String>> = Vec::with_capacity(args.parallel);
    for (i, wd) in desired.iter().enumerate() {
        let idx = 1000 + i;
        match create_workdir(mode, args, sources, wd, idx) {
            Ok(actual) => {
                workdirs.push(actual);
                branches.push(branch_name_for(mode, args, idx));
            }
            Err(e) => {
                // Roll back any already-created workdirs so we don't
                // leak FUSE mounts / git worktrees / branches when one
                // of the N stress workers fails to spin up.
                teardown_workdirs(mode, sources, &workdirs, &branches);
                return Err(e);
            }
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(args.stress_secs);

    let (tx, rx) = mpsc::channel::<(usize, u64, Vec<f64>, Vec<String>)>();
    let mut handles = Vec::new();
    for (i, wd) in workdirs.iter().cloned().enumerate() {
        let stop = stop.clone();
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            let mut iters: u64 = 0;
            let mut iter_times: Vec<f64> = Vec::new();
            let mut errs: Vec<String> = Vec::new();
            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                let r = run_cargo_killable(&wd, &["build", "--workspace"], &stop);
                let dt = t0.elapsed().as_secs_f64();
                match r {
                    Ok(()) => {
                        iter_times.push(dt);
                        iters += 1;
                        if touch_one_file(&wd).is_err() {
                            errs.push(format!("agent-{i}: touch_one_file failed"));
                            break;
                        }
                    }
                    Err(e) => {
                        // Cap error body to avoid log explosions when the
                        // same failure (e.g. ENOSYS from FUSE) repeats.
                        let body = format!("{e}");
                        let trimmed = body.lines().take(2).collect::<Vec<_>>().join(" | ");
                        let trimmed: String = trimmed.chars().take(240).collect::<String>();
                        errs.push(format!("agent-{i} iter {iters}: {trimmed}"));
                        if errs.len() > 5 {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send((i, iters, iter_times, errs));
        }));
    }
    drop(tx);

    // Watchdog: hard cap at deadline + 5 min in case a build hangs.
    let watchdog_stop = stop.clone();
    let watchdog_deadline = deadline + Duration::from_secs(300);
    thread::spawn(move || {
        while Instant::now() < watchdog_deadline {
            thread::sleep(Duration::from_secs(5));
            if watchdog_stop.load(Ordering::Relaxed) {
                return;
            }
        }
        watchdog_stop.store(true, Ordering::Relaxed);
    });

    let mut out = StressOutcome {
        duration_secs: args.stress_secs,
        parallel: args.parallel,
        mode: mode.as_str().to_string(),
        ..Default::default()
    };
    out.iters_per_child.resize(args.parallel, 0);
    out.first_iter_secs.resize(args.parallel, f64::NAN);
    out.last_iter_secs.resize(args.parallel, f64::NAN);
    while let Ok((i, iters, iter_times, errs)) = rx.recv() {
        out.iters_per_child[i] = iters;
        if let Some(&f) = iter_times.first() {
            out.first_iter_secs[i] = f;
        }
        if let Some(&l) = iter_times.last() {
            out.last_iter_secs[i] = l;
        }
        out.failures.extend(errs);
    }
    for h in handles {
        let _ = h.join();
    }

    // dmesg | tail for FUSE complaints (best-effort, often unprivileged).
    if let Ok(out_cmd) = Command::new("dmesg").arg("--ctime").output()
        && out_cmd.status.success()
    {
        let body = String::from_utf8_lossy(&out_cmd.stdout);
        for line in body.lines().rev().take(200) {
            let l = line.to_lowercase();
            if l.contains("fuse") || l.contains("oom") {
                out.fuse_dmesg_excerpts.push(line.to_string());
            }
        }
    }

    teardown_workdirs(mode, sources, &workdirs, &branches);
    Ok(out)
}

// ---------- Parallel runner ----------

fn run_parallel<F>(n: usize, f: F) -> ParallelTiming
where
    F: Fn(usize) -> Result<()> + Send + Sync,
{
    let mut per_child = vec![0.0_f64; n];
    let mut errors: Vec<Option<String>> = vec![None; n];
    let started = Instant::now();
    thread::scope(|s| {
        let mut handles = Vec::new();
        for i in 0..n {
            let f = &f;
            handles.push((
                i,
                s.spawn(move || {
                    let t0 = Instant::now();
                    let r = f(i);
                    (t0.elapsed().as_secs_f64(), r)
                }),
            ));
        }
        for (i, h) in handles {
            let (dt, r) = h.join().expect("thread panic");
            per_child[i] = dt;
            if let Err(e) = r {
                errors[i] = Some(format!("{e:#}"));
            }
        }
    });
    let aggregate = started.elapsed().as_secs_f64();
    let first_err = errors.iter().find_map(|e| e.clone());
    ParallelTiming {
        per_child_secs: per_child,
        aggregate_secs: aggregate,
        all_ok: first_err.is_none(),
        error: first_err,
    }
}

// ---------- Process helpers ----------

fn sh(prog: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Result<()> {
    let st = Command::new(&prog)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status_with_stderr()?;
    check(&st.0, &st.1, &format!("{:?} {:?}", prog.as_ref(), args))
}

fn run_in(dir: &Path, prog: &str, args: &[&str]) -> Result<()> {
    let st = Command::new(prog)
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status_with_stderr()?;
    check(
        &st.0,
        &st.1,
        &format!("{prog} {args:?} (in {})", dir.display()),
    )
}

/// Like [`run_cargo`], but polls `stop` while cargo runs and escalates
/// to SIGTERM → 5s grace → SIGKILL when the watchdog fires. Used by the
/// stress loop so a wedged cargo subprocess can't pin a worker thread
/// past the watchdog deadline.
fn run_cargo_killable(workdir: &Path, args: &[&str], stop: &AtomicBool) -> Result<()> {
    use std::io::Read as _;

    let target = workdir.join(".cargo-target");
    let mut child = Command::new("cargo")
        .current_dir(workdir)
        .env("CARGO_TARGET_DIR", &target)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id() as i32;

    // Drain stderr concurrently to avoid the pipe filling and deadlocking
    // a build that emits a lot of warnings.
    let stderr_pipe = child.stderr.take();
    let stderr_handle = stderr_pipe.map(|mut s| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });

    let label = format!("cargo {args:?} (in {})", workdir.display());

    loop {
        match child.try_wait()? {
            Some(status) => {
                let stderr = stderr_handle
                    .and_then(|h| h.join().ok())
                    .unwrap_or_default();
                return check(&status, &stderr, &label);
            }
            None => {
                if stop.load(Ordering::Relaxed) {
                    // SIGTERM → 5s grace → SIGKILL.
                    // SAFETY: kill(2) with a positive pid + signal is a
                    // C-stable call that doesn't touch Rust memory; the
                    // pid came from this Child and won't be reused
                    // while the Child handle is alive.
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                    let grace = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < grace {
                        if child.try_wait()?.is_some() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                    if child.try_wait()?.is_none() {
                        let _ = child.kill();
                    }
                    let _ = child.wait();
                    if let Some(h) = stderr_handle {
                        let _ = h.join();
                    }
                    bail!("{label}: terminated by stress watchdog");
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn run_cargo(workdir: &Path, args: &[&str]) -> Result<()> {
    let target = workdir.join(".cargo-target");
    let st = Command::new("cargo")
        .current_dir(workdir)
        .env("CARGO_TARGET_DIR", &target)
        // Disable incremental compilation interference between
        // measurements — cargo's per-target-dir incremental shape is
        // enabled by default; we only want it for the incremental-check
        // step, not the cold ones. (But it gets cleared between cold
        // measurements anyway because we delete .cargo-target.)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status_with_stderr()?;
    check(
        &st.0,
        &st.1,
        &format!("cargo {args:?} (in {})", workdir.display()),
    )
}

trait StatusWithStderr {
    fn status_with_stderr(&mut self) -> Result<(ExitStatus, Vec<u8>)>;
}
impl StatusWithStderr for Command {
    fn status_with_stderr(&mut self) -> Result<(ExitStatus, Vec<u8>)> {
        let out = self.output()?;
        Ok((out.status, out.stderr))
    }
}

fn check(status: &ExitStatus, stderr: &[u8], label: &str) -> Result<()> {
    if !status.success() {
        let tail = String::from_utf8_lossy(stderr);
        let tail = tail.lines().rev().take(20).collect::<Vec<_>>();
        bail!(
            "{label} failed: exit={:?}\n  {}",
            status,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n  ")
        );
    }
    Ok(())
}

// ---------- FS helpers ----------

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    let st = Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", src.display()))
        .arg(dst)
        .status()?;
    if !st.success() {
        bail!("cp -a {} -> {} failed", src.display(), dst.display());
    }
    Ok(())
}

fn copy_tree_contents(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    // Copy everything under src/* into dst, preserving attrs. Skip .git.
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || name == ".heddle" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let st = Command::new("cp").arg("-a").arg(&from).arg(&to).status()?;
        if !st.success() {
            bail!("cp -a {} -> {} failed", from.display(), to.display());
        }
    }
    Ok(())
}

fn touch_one_file(workdir: &Path) -> Result<()> {
    // Find the first .rs file under src/ of the workspace root or any
    // crate and append a `//` comment line. Touch alone is not enough
    // for cargo incremental — it keys on content + mtime.
    let candidates = [workdir.join("src/lib.rs"), workdir.join("src/main.rs")];
    for c in &candidates {
        if c.is_file() {
            return append_touch_line(c);
        }
    }
    // Walk every Cargo.toml under the workdir and look at the src/
    // sibling. This catches workspaces whose members are not under
    // `crates/*` — e.g. `members = ["packages/*", "tools/*"]`, or a
    // flat layout. We use walkdir (already a workspace dep) with a
    // depth cap so we don't recurse into deps' `target/` if one
    // exists, and skip the cargo target dir explicitly.
    for entry in walkdir::WalkDir::new(workdir)
        .max_depth(6)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            // Skip noisy dirs that can't host workspace members.
            !matches!(
                name.as_ref(),
                "target" | ".cargo-target" | ".git" | ".heddle" | "node_modules"
            )
        })
        .flatten()
    {
        if entry.file_name() != "Cargo.toml" {
            continue;
        }
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        for sub in ["src/lib.rs", "src/main.rs"] {
            let p = parent.join(sub);
            if p.is_file() {
                return append_touch_line(&p);
            }
        }
    }
    bail!("no .rs file found to touch in {}", workdir.display())
}

fn append_touch_line(p: &Path) -> Result<()> {
    let mut f = fs::OpenOptions::new().append(true).open(p)?;
    writeln!(f, "// bench touch {}", uniq())?;
    Ok(())
}

fn uniq() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn dir_size_bytes(p: &Path) -> u64 {
    let out = Command::new("du").args(["-sb"]).arg(p).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

// ---------- Output ----------

fn print_summary(
    env: &Environment,
    args: &Args,
    results: &[ModeResult],
    stress: &Option<StressOutcome>,
) {
    eprintln!("================ summary ================");
    eprintln!("host: {}", env.summary_line());
    eprintln!("parallel: {}", args.parallel);
    eprintln!();
    for r in results {
        eprintln!("[mode={}]", r.mode);
        eprintln!("  create           : {}", fmt_par(&r.create));
        eprintln!("  cold check       : {}", fmt_par(&r.cold_check));
        eprintln!("  incremental check: {}", fmt_par(&r.incremental_check));
        eprintln!("  cold release     : {}", fmt_par(&r.cold_release));
        eprintln!(
            "  disk post-create : {}",
            r.disk_post_create
                .iter()
                .map(|b| human_bytes(*b))
                .collect::<Vec<_>>()
                .join(", ")
        );
        eprintln!(
            "  disk post-build  : {}",
            r.disk_post_build
                .iter()
                .map(|b| human_bytes(*b))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for n in &r.notes {
            eprintln!("  note: {n}");
        }
    }
    if let Some(s) = stress {
        eprintln!();
        eprintln!(
            "[stress mode={} duration={}s parallel={}]",
            s.mode, s.duration_secs, s.parallel
        );
        eprintln!("  iters/child   : {:?}", s.iters_per_child);
        eprintln!("  first iter s  : {:?}", s.first_iter_secs);
        eprintln!("  last iter s   : {:?}", s.last_iter_secs);
        eprintln!("  failures      : {}", s.failures.len());
        for f in s.failures.iter().take(5) {
            eprintln!("    - {f}");
        }
        if !s.fuse_dmesg_excerpts.is_empty() {
            eprintln!("  fuse dmesg lines: {}", s.fuse_dmesg_excerpts.len());
        }
    }
}

fn fmt_par(p: &ParallelTiming) -> String {
    if !p.all_ok {
        return format!(
            "FAILED ({}): per-child={:?}",
            p.error.as_deref().unwrap_or("?"),
            p.per_child_secs
        );
    }
    let per: Vec<String> = p
        .per_child_secs
        .iter()
        .map(|v| format!("{v:.2}s"))
        .collect();
    format!(
        "aggregate {:.2}s | per-child {}",
        p.aggregate_secs,
        per.join(",")
    )
}

fn human_bytes(b: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let f = b as f64;
    if f >= GIB {
        format!("{:.2} GiB", f / GIB)
    } else if f >= MIB {
        format!("{:.1} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.0} KiB", f / KIB)
    } else {
        format!("{b} B")
    }
}

fn write_json(
    path: &Path,
    env: &Environment,
    args: &Args,
    results: &[ModeResult],
    stress: &Option<StressOutcome>,
) -> Result<()> {
    // Hand-rolled (no serde dep in devtools) — simple shape, never
    // intended for arbitrary consumers.
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!("  \"host\": {},\n", json_obj_env(env)));
    s.push_str(&format!("  \"parallel\": {},\n", args.parallel));
    s.push_str(&format!(
        "  \"modes_requested\": {},\n",
        json_str_array(
            &args
                .modes
                .iter()
                .map(|m| m.as_str().to_string())
                .collect::<Vec<_>>()
        )
    ));
    s.push_str("  \"results\": [\n");
    for (i, r) in results.iter().enumerate() {
        s.push_str("    ");
        s.push_str(&json_obj_mode(r));
        if i + 1 < results.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  ],\n");
    if let Some(st) = stress {
        s.push_str(&format!("  \"stress\": {}\n", json_obj_stress(st)));
    } else {
        s.push_str("  \"stress\": null\n");
    }
    s.push_str("}\n");
    fs::write(path, s)?;
    Ok(())
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_str_array(v: &[String]) -> String {
    let inner: Vec<String> = v.iter().map(|s| json_str(s)).collect();
    format!("[{}]", inner.join(","))
}

fn json_f64_array(v: &[f64]) -> String {
    let inner: Vec<String> = v
        .iter()
        .map(|x| {
            if x.is_finite() {
                format!("{x}")
            } else {
                "null".to_string()
            }
        })
        .collect();
    format!("[{}]", inner.join(","))
}

fn json_u64_array(v: &[u64]) -> String {
    let inner: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", inner.join(","))
}

fn json_obj_env(e: &Environment) -> String {
    format!(
        "{{ \"kernel\": {}, \"distro\": {}, \"cpu_model\": {}, \"cpu_threads\": {}, \"mem_total_kb\": {}, \"fs_type\": {}, \"cargo_version\": {} }}",
        json_str(&e.kernel),
        json_str(&e.distro),
        json_str(&e.cpu_model),
        e.cpu_threads,
        e.mem_total_kb,
        json_str(&e.fs_type),
        json_str(&e.cargo_version),
    )
}

fn json_obj_par(p: &ParallelTiming) -> String {
    format!(
        "{{ \"per_child_secs\": {}, \"aggregate_secs\": {}, \"all_ok\": {}, \"error\": {} }}",
        json_f64_array(&p.per_child_secs),
        if p.aggregate_secs.is_finite() {
            format!("{}", p.aggregate_secs)
        } else {
            "null".to_string()
        },
        p.all_ok,
        match &p.error {
            Some(e) => json_str(e),
            None => "null".to_string(),
        }
    )
}

fn json_obj_mode(r: &ModeResult) -> String {
    format!(
        "{{ \"mode\": {}, \"create\": {}, \"cold_check\": {}, \"incremental_check\": {}, \"cold_release\": {}, \"disk_post_create\": {}, \"disk_post_build\": {}, \"notes\": {} }}",
        json_str(&r.mode),
        json_obj_par(&r.create),
        json_obj_par(&r.cold_check),
        json_obj_par(&r.incremental_check),
        json_obj_par(&r.cold_release),
        json_u64_array(&r.disk_post_create),
        json_u64_array(&r.disk_post_build),
        json_str_array(&r.notes),
    )
}

fn json_obj_stress(s: &StressOutcome) -> String {
    format!(
        "{{ \"mode\": {}, \"duration_secs\": {}, \"parallel\": {}, \"iters_per_child\": {}, \"first_iter_secs\": {}, \"last_iter_secs\": {}, \"failures\": {}, \"fuse_dmesg_excerpts\": {} }}",
        json_str(&s.mode),
        s.duration_secs,
        s.parallel,
        json_u64_array(&s.iters_per_child),
        json_f64_array(&s.first_iter_secs),
        json_f64_array(&s.last_iter_secs),
        json_str_array(&s.failures),
        json_str_array(&s.fuse_dmesg_excerpts),
    )
}

fn write_markdown(
    path: &Path,
    env: &Environment,
    args: &Args,
    results: &[ModeResult],
    stress: &Option<StressOutcome>,
) -> Result<()> {
    let mut s = String::new();
    s.push_str("# fuse-dispatch-bench results\n\n");
    s.push_str(&format!("- Host: `{}`\n", env.summary_line()));
    s.push_str(&format!("- Cargo: `{}`\n", env.cargo_version));
    s.push_str(&format!("- Parallel agents: `{}`\n", args.parallel));
    s.push_str(&format!("- Workload: `{}`\n", args.workload.display()));
    s.push_str(&format!(
        "- Heddle bin: `{}`\n\n",
        args.heddle_bin.display()
    ));

    s.push_str("## Per-mode results\n\n");
    s.push_str("| mode | create agg | cold check agg | incr check agg | cold release agg | disk post-build (sum) |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for r in results {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.mode,
            par_or_fail(&r.create),
            par_or_fail(&r.cold_check),
            par_or_fail(&r.incremental_check),
            par_or_fail(&r.cold_release),
            human_bytes(r.disk_post_build.iter().sum()),
        ));
    }
    s.push_str("\n### Per-child breakdown\n\n");
    for r in results {
        s.push_str(&format!("**{}**\n\n", r.mode));
        s.push_str("| metric | per-child seconds |\n|---|---|\n");
        s.push_str(&format!(
            "| create | {} |\n",
            csv_secs(&r.create.per_child_secs)
        ));
        s.push_str(&format!(
            "| cold check | {} |\n",
            csv_secs(&r.cold_check.per_child_secs)
        ));
        s.push_str(&format!(
            "| incr check | {} |\n",
            csv_secs(&r.incremental_check.per_child_secs)
        ));
        s.push_str(&format!(
            "| cold release | {} |\n",
            csv_secs(&r.cold_release.per_child_secs)
        ));
        s.push_str(&format!(
            "| disk post-create per child | {} |\n",
            r.disk_post_create
                .iter()
                .map(|b| human_bytes(*b))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        s.push_str(&format!(
            "| disk post-build per child | {} |\n\n",
            r.disk_post_build
                .iter()
                .map(|b| human_bytes(*b))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        for n in &r.notes {
            s.push_str(&format!("> note: {n}\n"));
        }
    }

    if let Some(st) = stress {
        s.push_str("\n## Stress test\n\n");
        s.push_str(&format!(
            "Mode `{}`, {} parallel agents, ran for {}s.\n\n",
            st.mode, st.parallel, st.duration_secs
        ));
        s.push_str("| metric | per child |\n|---|---|\n");
        s.push_str(&format!("| iters completed | {:?} |\n", st.iters_per_child));
        s.push_str(&format!(
            "| first iter (s) | {} |\n",
            csv_secs(&st.first_iter_secs)
        ));
        s.push_str(&format!(
            "| last iter (s) | {} |\n",
            csv_secs(&st.last_iter_secs)
        ));
        s.push_str(&format!("| failures | {} |\n\n", st.failures.len()));
        if !st.failures.is_empty() {
            s.push_str("Failures:\n");
            for f in &st.failures {
                s.push_str(&format!("- {f}\n"));
            }
            s.push('\n');
        }
        if !st.fuse_dmesg_excerpts.is_empty() {
            s.push_str("FUSE-related dmesg lines:\n```\n");
            for l in &st.fuse_dmesg_excerpts {
                s.push_str(l);
                s.push('\n');
            }
            s.push_str("```\n");
        }
    }

    fs::write(path, s)?;
    Ok(())
}

fn par_or_fail(p: &ParallelTiming) -> String {
    if !p.all_ok {
        "FAIL".to_string()
    } else if p.aggregate_secs.is_finite() {
        format!("{:.2}s", p.aggregate_secs)
    } else {
        "—".to_string()
    }
}

fn csv_secs(v: &[f64]) -> String {
    v.iter()
        .map(|x| {
            if x.is_finite() {
                format!("{x:.2}")
            } else {
                "—".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
