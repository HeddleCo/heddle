// SPDX-License-Identifier: Apache-2.0
//! Local synthetic fixture and native source adapter; never connects to hosted services.
use heddle_git_projection::gateway_view::{
    ViewLimits, export_public_native_view, export_public_native_view_with_authorized_threads,
};
use objects::object::{Attribution, AudienceTier, Principal, StateId, ThreadName, VisibilityTier};
use repo::Repository;
use std::{
    path::Path,
    sync::{Arc, Barrier},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn actor() -> Attribution {
    Attribution::human(Principal::new("Synthetic Demo", "demo@example.invalid"))
}
fn fixture(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let native = root.join("native");
    let repo = Repository::init_default(&native)?;
    std::fs::write(
        native.join("README.md"),
        "Synthetic native Heddle fixture\n",
    )?;
    let base = repo.snapshot_with_attribution(Some("base".into()), None, actor())?;
    for name in ["agent-a", "agent-b"] {
        repo.create_native_thread(
            name,
            base.state_id,
            Some("main"),
            "synthetic concurrent edit",
        )?;
        repo.set_thread_recorded(&ThreadName::new(name), &base.state_id)?;
        repo.materialize_thread(name, &root.join(name), &AudienceTier::Public)?;
    }
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for name in ["agent-a", "agent-b"] {
        let root = root.to_path_buf();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || -> Result<()> {
            let checkout = root.join(name);
            std::fs::write(
                checkout.join(format!("{name}.txt")),
                format!("Concurrent edit from {name}\n"),
            )?;
            barrier.wait();
            let repo = Repository::open(root.join("native"))?;
            repo.capture_thread_from_disk(name, &checkout)?;
            Ok(())
        }));
    }
    for worker in workers {
        worker.join().map_err(|_| "fixture worker panicked")??;
    }
    let a = repo
        .refs()
        .get_thread(&ThreadName::new("agent-a"))?
        .ok_or("missing a")?;
    let b = repo
        .refs()
        .get_thread(&ThreadName::new("agent-b"))?
        .ok_or("missing b")?;
    // Explicit synthetic integration: combine two disjoint files, then use the native
    // snapshot chokepoint. This is a derived view, not a native Thread merge.
    for name in ["agent-a", "agent-b"] {
        std::fs::copy(
            root.join(name).join(format!("{name}.txt")),
            native.join(format!("{name}.txt")),
        )?;
    }
    let merged = repo.snapshot_with_attribution(
        Some("derived concurrent view; inputs agent-a and agent-b".into()),
        None,
        actor(),
    )?;
    let ordered = repo.snapshot_merge_with_attribution(
        &base.state_id,
        Some("ordered parent fixture".into()),
        None,
        actor(),
        Some(base.state_id),
        true,
    )?;
    std::fs::write(
        native.join("restricted.txt"),
        "Unpublished restricted fixture bytes\n",
    )?;
    repo.mark_entry_visibility("restricted.txt", VisibilityTier::Internal)?;
    let restricted =
        repo.snapshot_with_attribution(Some("restricted fixture".into()), None, actor())?;
    println!(
        "{}",
        serde_json::json!({"ordered": ordered.state_id.to_string_full(), "restricted": restricted.state_id.to_string_full(), "base": base.state_id.to_string_full(), "a": a.to_string_full(), "b": b.to_string_full(), "merged": merged.state_id.to_string_full()})
    );
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("format") if args.len() == 2 => {
            println!("{}", repo::RepoConfig::default().repository.version);
            Ok(())
        }
        Some("fixture") if args.len() == 3 => fixture(Path::new(&args[2])),
        Some("export") if args.len() == 7 || args.len() == 8 => {
            let repo = Repository::open(&args[2])?;
            let tip = StateId::parse(&args[3])?;
            let snapshot = match args[5].as_str() { "snapshot" => true, "history" => false, _ => return Err("invalid view mode".into()) };
            if Path::new(&args[4]).exists() { return Err("sink must not exist".into()); }
            let sink = sley::Repository::init_bare(&args[4])?;
            let oid = if let Some(names) = args.get(7) {
                if names.len() > 32768 { return Err("authorized Thread list limit".into()); }
                let names: Vec<String> = serde_json::from_str(names)?;
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                export_public_native_view_with_authorized_threads(&repo, &sink, tip, &args[6], &names, snapshot, ViewLimits::default())?
            } else {
                export_public_native_view(&repo, &sink, tip, &args[6], snapshot, ViewLimits::default())?
            };
            println!("{oid}"); Ok(())
        }
        _ => Err("usage: gateway_native format | fixture DIR | export NATIVE STATE NEW_BARE_DIR snapshot|history THREAD [AUTHORIZED_THREAD_NAMES_JSON]".into())
    }
}
