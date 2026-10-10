// SPDX-License-Identifier: Apache-2.0
//! Local synthetic agent rehearsal through public native APIs, never hosted APIs.
//! This is not the CLI's ready/land workflow or an automatic merge engine.
use objects::{
    object::{
        Annotation, AnnotationKind, AnnotationScope, AudienceTier, ContextBlob, ContextTarget,
        State, StateAttachment, StateAttachmentBody, StateId, ThreadName, VisibilityTier,
    },
    store::ObjectStore,
};
use repo::Repository;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const FILTER: &str = "demo/open-filter";
const SORT: &str = "demo/priority-sort";

fn checkout(name: &str) -> Result<&'static str> {
    match name {
        FILTER => Ok("agent-filter"),
        SORT => Ok("agent-sort"),
        _ => Err("only the two synthetic demo Threads are supported".into()),
    }
}

fn head(repo: &Repository, name: &str) -> Result<StateId> {
    Ok(repo
        .refs()
        .get_thread(&ThreadName::new(name))?
        .ok_or("missing demo Thread")?)
}

fn prepare(root: &Path) -> Result<()> {
    let native = root.join("native");
    if !native.join("app.js").is_file() || native.join(".heddle").exists() {
        return Err("copy the public toy baseline to a NEW root/native first".into());
    }
    let repo = Repository::init_default(&native)?;
    let base = repo.snapshot_with_attribution(
        Some("Fresh v6 two-agent demo baseline".into()),
        None,
        repo.get_attribution()?,
    )?;
    for name in [FILTER, SORT] {
        repo.create_native_thread(
            name,
            base.state_id,
            Some("main"),
            "isolated actual coding-agent task",
        )?;
        repo.set_thread_recorded(&ThreadName::new(name), &base.state_id)?;
        repo.materialize_thread(name, &root.join(checkout(name)?), &AudienceTier::Public)?;
    }
    println!(
        "{}",
        serde_json::json!({"base": base.state_id.to_string_full(), "threads": [FILTER, SORT]})
    );
    Ok(())
}

fn capture(root: &Path, name: &str) -> Result<()> {
    let worktree = root.join(checkout(name)?);
    let repo = Repository::open(root.join("native"))?;
    if repo.get_attribution()?.agent.is_none() {
        return Err("agent attribution must be configured before capture".into());
    }
    let outcome = repo.capture_thread_from_disk(name, &worktree)?;
    let id = head(&repo, name)?;
    let operation = repo.record_native_capture(name, id)?;
    let state = repo
        .store()
        .get_state(&id)?
        .ok_or("captured state unavailable")?;
    println!(
        "{}",
        serde_json::json!({"thread": name, "state": id.to_string_full(),
        "operation": operation.to_string(), "outcome": format!("{outcome:?}"), "attribution": state.attribution})
    );
    Ok(())
}

fn integrate(root: &Path, combined: &Path) -> Result<()> {
    let repo = Repository::open(root.join("native"))?;
    let source = head(&repo, FILTER)?;
    let target = head(&repo, SORT)?;
    if source == target {
        return Err("both agents must capture a distinct change first".into());
    }
    let source_replica = repo.native_thread(FILTER)?;
    let originals = source_replica.accepted_source_originals_for_revisions(&[source])?;
    if originals.len() != 1 {
        return Err("expected one exact source capture".into());
    }
    let source_operation = originals[0].1.verify()?.id()?;
    let left = repo.store().get_state(&target)?.ok_or("missing target")?;
    let right = repo.store().get_state(&source)?.ok_or("missing source")?;
    let tree = repo.build_tree(combined)?;
    let tree_hash = repo.store().put_tree(&tree)?;
    let state = State::new_merge(tree_hash, vec![target, source], repo.get_attribution()?)
        .with_intent("Explicit coordinator composition of two actual agents; not automatic conflict resolution");
    let parent_context = repo.union_parent_contexts(&[&left, &right])?;
    let context_target = ContextTarget::File {
        path: "app.js".into(),
    };
    let mut context = match parent_context {
        Some(hash) => repo
            .get_context_blob(&hash, &context_target)?
            .unwrap_or_else(|| ContextBlob::new(Vec::new())),
        None => ContextBlob::new(Vec::new()),
    };
    context.annotations.push(Annotation::new(
            AnnotationScope::File, AnnotationKind::Invariant,
            "Open-only filtering and stable priority sorting compose; default order and input data remain unchanged.".into(),
            vec!["demo-review".into()], "Synthetic demo coordinator".into(),
            chrono::Utc::now().timestamp(), None, None, VisibilityTier::Public,
        ));
    let context_root = repo.set_context_blob(parent_context.as_ref(), &context_target, &context)?;
    repo.put_authored_state(&state)?;
    repo.put_state_attachment(&StateAttachment {
        state_id: state.id(),
        body: StateAttachmentBody::Context(context_root),
        attribution: state.attribution.clone(),
        created_at: chrono::Utc::now(),
        supersedes: None,
    })?;
    let operation = repo.record_native_local_integration(
        SORT,
        state.id(),
        source_replica.thread_id(),
        source_operation,
        source,
    )?;
    repo.set_thread_recorded(&ThreadName::new(SORT), &state.id())?;
    println!(
        "{}",
        serde_json::json!({"thread": SORT, "state": state.id().to_string_full(),
        "operation": operation.to_string(), "parents": [target.to_string_full(), source.to_string_full()],
        "context": context_root.to_string(),
        "semantics": "explicit composition admitted on demo/priority-sort; native main is unchanged"})
    );
    Ok(())
}

fn annotate(root: &Path, name: &str, note: &Path) -> Result<()> {
    checkout(name)?;
    if std::fs::metadata(note)?.len() > 8192 {
        return Err("annotation text limit".into());
    }
    let content = std::fs::read_to_string(note)?;
    if content.trim().is_empty() {
        return Err("nonempty actual agent invariant required".into());
    }
    let repo = Repository::open(root.join("native"))?;
    let state = repo
        .store()
        .get_state(&head(&repo, name)?)?
        .ok_or("missing captured state")?;
    let attribution = repo.get_attribution()?;
    if attribution.agent.is_none() {
        return Err("agent attribution must be configured".into());
    }
    let context = ContextBlob::new(vec![Annotation::new(
        AnnotationScope::File,
        AnnotationKind::Invariant,
        content,
        vec!["agent-invariant".into()],
        std::str::from_utf8(&attribution.principal.name)?.to_owned(),
        chrono::Utc::now().timestamp(),
        None,
        Some(state.id()),
        VisibilityTier::Public,
    )]);
    let context_root = repo.set_context_blob(
        None,
        &ContextTarget::File {
            path: "app.js".into(),
        },
        &context,
    )?;
    let attachment = repo.put_state_attachment(&StateAttachment {
        state_id: state.id(),
        body: StateAttachmentBody::Context(context_root),
        attribution,
        created_at: chrono::Utc::now(),
        supersedes: None,
    })?;
    println!(
        "{}",
        serde_json::json!({"thread": name, "state": state.id().to_string_full(),
        "context": context_root.to_string(), "attachment": attachment.to_string()})
    );
    Ok(())
}

fn inspect(root: &Path, id: StateId) -> Result<()> {
    let repo = Repository::open(root.join("native"))?;
    let state = repo.store().get_state(&id)?.ok_or("missing state")?;
    let context = match repo.inherit_parent_context(&state)? {
        Some(hash) => repo
            .list_context_entries(&hash, None)?
            .into_iter()
            .map(|entry| serde_json::json!({"target": entry.target, "blob": entry.blob}))
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    println!(
        "{}",
        serde_json::json!({"state": state.id().to_string_full(), "tree": state.tree.to_string(),
        "parents": state.parents.iter().map(StateId::to_string_full).collect::<Vec<_>>(),
        "attribution": state.attribution, "context": context})
    );
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("prepare") if args.len() == 3 => prepare(Path::new(&args[2])),
        Some("capture") if args.len() == 4 => capture(Path::new(&args[2]), &args[3]),
        Some("annotate") if args.len() == 5 => annotate(Path::new(&args[2]), &args[3], Path::new(&args[4])),
        Some("integrate-reviewed") if args.len() == 4 => integrate(Path::new(&args[2]), Path::new(&args[3])),
        Some("inspect") if args.len() == 4 => inspect(Path::new(&args[2]), StateId::parse(&args[3])?),
        _ => Err("usage: gateway_agent_demo prepare ROOT | capture ROOT THREAD | annotate ROOT THREAD NOTE_FILE | integrate-reviewed ROOT COMPOSED_DIRECTORY | inspect ROOT STATE".into()),
    }
}
