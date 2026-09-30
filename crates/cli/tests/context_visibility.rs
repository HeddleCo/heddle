// SPDX-License-Identifier: Apache-2.0
//! Conversion, replacement, and transport must preserve the decision's audience.

#[path = "support/mod.rs"]
mod support;
use objects::object::{Annotation, ContextTarget, VisibilityTier};
use support::*;

fn fixture() -> TempDir {
    let temp = TempDir::new().expect("repository");
    heddle(&["init"], Some(temp.path())).expect("init");
    seed_test_repo_principal(temp.path()).expect("principal");
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").expect("source");
    heddle(&["capture", "-m", "seed"], Some(temp.path())).expect("capture");
    std::fs::create_dir_all(temp.path().join(".heddle/visibility")).expect("audience directory");
    std::fs::write(
        temp.path().join(".heddle/visibility/audience_label"),
        "preview-evaluation",
    )
    .expect("local scope membership");
    temp
}

fn json(path: &Path, args: &[&str]) -> Value {
    let mut argv = vec!["--output", "json"];
    argv.extend_from_slice(args);
    serde_json::from_str(&heddle(&argv, Some(path)).expect("CLI success")).expect("JSON")
}

fn open(path: &Path, visibility: &str) -> String {
    json(
        path,
        &[
            "discuss",
            "new",
            "--path",
            "main.rs",
            "--visibility",
            visibility,
            "--body",
            "Preserve the evaluation decision",
        ],
    )["discussion"]["id"]
        .as_str()
        .expect("discussion id")
        .to_owned()
}

fn annotations(path: &Path) -> Vec<Annotation> {
    let repo = Repository::open(path).expect("repository");
    let head = repo.current_state().expect("HEAD").expect("state");
    let root = repo
        .inherit_parent_context(&head)
        .expect("context")
        .expect("root");
    repo.get_context_blob(&root, &ContextTarget::file("main.rs").expect("target"))
        .expect("blob")
        .expect("annotations")
        .annotations
}

fn resolve(path: &Path, id: &str, extra: &[&str]) -> Value {
    let mut args = vec![
        "discuss",
        "resolve",
        id,
        "--mode",
        "into-annotation",
        "--kind",
        "invariant",
        "--body",
        "The evaluation decision is confidential",
    ];
    args.extend_from_slice(extra);
    json(path, &args)
}

#[test]
fn private_discussion_conversion_preserves_visibility_in_context_outputs() {
    let temp = fixture();
    let id = open(temp.path(), "private:preview-evaluation");
    resolve(temp.path(), &id, &[]);
    assert_eq!(
        annotations(temp.path())[0].visibility,
        VisibilityTier::Private {
            scope_label: "preview-evaluation".into()
        },
        "conversion broadened the source discussion audience"
    );
    let get = json(temp.path(), &["context", "get", "--path", "main.rs"]);
    assert_eq!(
        get["annotations"][0]["visibility"],
        "private:preview-evaluation"
    );
    let text =
        heddle(&["context", "get", "--path", "main.rs"], Some(temp.path())).expect("text context");
    assert!(text.contains("private:preview-evaluation"), "{text}");
    let brief = json(temp.path(), &["context", "--for-thread", "main"]);
    assert_eq!(
        brief["annotations"][0]["visibility"],
        "private:preview-evaluation"
    );
    let text =
        heddle(&["context", "--for-thread", "main"], Some(temp.path())).expect("text briefing");
    assert!(text.contains("private:preview-evaluation"), "{text}");
    std::fs::remove_file(temp.path().join(".heddle/visibility/audience_label"))
        .expect("remove membership");
    assert!(
        json(temp.path(), &["context", "--for-thread", "main"])["annotations"]
            .as_array()
            .expect("annotations")
            .is_empty()
    );
}

#[test]
fn conversion_rejects_widening_and_rescoping_before_writing() {
    let temp = fixture();
    for (source, requested) in [
        ("private:preview-evaluation", "public"),
        (
            "private:preview-evaluation",
            "restricted:preview-evaluation",
        ),
        ("private:preview-evaluation", "private:other"),
        ("team:engineering", "restricted:preview-evaluation"),
        ("internal", "restricted:preview-evaluation"),
    ] {
        let id = open(temp.path(), source);
        let output = heddle_output(
            &[
                "--output",
                "json",
                "discuss",
                "resolve",
                &id,
                "--mode",
                "into-annotation",
                "--body",
                "decision",
                "--visibility",
                requested,
            ],
            Some(temp.path()),
        )
        .expect("command");
        assert!(
            !output.status.success(),
            "{source} -> {requested} widened visibility"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("discuss_annotation_visibility_widening"),
            "{stderr}"
        );
        assert!(
            json(temp.path(), &["context", "get", "--path", "main.rs"])["annotations"]
                .as_array()
                .expect("annotations")
                .is_empty()
        );
        assert_eq!(
            json(temp.path(), &["discuss", "show", &id])["discussion"]["status"],
            "open"
        );
    }
}

#[test]
fn conversion_honours_narrower_visibility() {
    let temp = fixture();
    let id = open(temp.path(), "restricted:preview-evaluation");
    resolve(
        temp.path(),
        &id,
        &["--visibility", "private:preview-evaluation"],
    );
    assert_eq!(
        annotations(temp.path())[0].visibility,
        VisibilityTier::Private {
            scope_label: "preview-evaluation".into()
        }
    );
}

#[test]
fn restricted_discussion_conversion_inherits_the_same_label() {
    let temp = fixture();
    let id = open(temp.path(), "restricted:preview-evaluation");
    resolve(temp.path(), &id, &[]);
    assert_eq!(
        annotations(temp.path())[0].visibility,
        VisibilityTier::Restricted {
            scope_label: "preview-evaluation".into()
        }
    );
}

#[test]
fn repeated_conversion_keeps_a_narrowed_annotation_and_rejects_further_narrowing_without_an_edit() {
    let temp = fixture();
    let id = open(temp.path(), "public");
    let initial = resolve(
        temp.path(),
        &id,
        &["--visibility", "restricted:preview-evaluation"],
    );
    let repeated = resolve(temp.path(), &id, &[]);
    assert_eq!(
        initial["discussion"]["resolution"]["annotation_id"],
        repeated["discussion"]["resolution"]["annotation_id"]
    );
    let output = heddle_output(
        &[
            "--output",
            "json",
            "discuss",
            "resolve",
            &id,
            "--mode",
            "into-annotation",
            "--body",
            "decision",
            "--visibility",
            "private:preview-evaluation",
        ],
        Some(temp.path()),
    )
    .expect("command");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("discuss_annotation_visibility_conflict")
    );
    assert_eq!(annotations(temp.path()).len(), 1);
    assert_eq!(
        annotations(temp.path())[0].visibility,
        VisibilityTier::Restricted {
            scope_label: "preview-evaluation".into()
        }
    );
}

#[test]
fn private_converted_annotation_survives_push_and_fresh_clone() {
    let temp = fixture();
    let id = open(temp.path(), "private:preview-evaluation");
    resolve(temp.path(), &id, &[]);
    let authored = annotations(temp.path())[0].clone();
    // A new source state exercises context inheritance and capture-time travel.
    std::fs::write(temp.path().join("other.rs"), "fn other() {}\n").expect("new source");
    heddle(
        &["capture", "-m", "advance with confidential decision"],
        Some(temp.path()),
    )
    .expect("capture");
    assert_eq!(annotations(temp.path())[0].visibility, authored.visibility);
    let transport = TempDir::new().expect("native remote");
    let remote = transport.path().join("remote");
    std::fs::create_dir(&remote).expect("remote directory");
    json(&remote, &["init"]);
    let remote_path = remote.to_str().expect("remote path");
    json(temp.path(), &["remote", "add", "round-trip", remote_path]);
    json(temp.path(), &["push", "round-trip"]);
    let clone = transport.path().join("fresh-clone");
    let clone_home = TempDir::new().expect("fresh clone home");
    let env = [(
        "HEDDLE_HOME",
        clone_home.path().to_str().expect("home path"),
    )];
    heddle_env(
        &[
            "--output",
            "json",
            "clone",
            remote_path,
            clone.to_str().expect("clone path"),
        ],
        Some(transport.path()),
        &env,
    )
    .expect("fresh native clone");
    let replicated = annotations(&clone);
    assert_eq!(replicated.len(), 1);
    assert_eq!(replicated[0], authored);
    let out = heddle_env(
        &["--output", "json", "context", "get", "--path", "main.rs"],
        Some(&clone),
        &env,
    )
    .expect("cloned context");
    let get: Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(
        get["annotations"][0]["visibility"],
        "private:preview-evaluation"
    );
}

#[test]
fn edit_and_supersede_preserve_private_visibility() {
    let temp = fixture();
    let id = open(temp.path(), "private:preview-evaluation");
    let resolved = resolve(temp.path(), &id, &[]);
    let annotation_id = resolved["discussion"]["resolution"]["annotation_id"]
        .as_str()
        .expect("annotation id");
    // Seed independently so this test also detects the replacement defect on integration.
    let repo = Repository::open(temp.path()).expect("repository");
    let head = repo.current_state().expect("HEAD").expect("state");
    let root = repo
        .inherit_parent_context(&head)
        .expect("context")
        .expect("root");
    let target = ContextTarget::file("main.rs").expect("target");
    let mut blob = repo
        .get_context_blob(&root, &target)
        .expect("blob")
        .expect("context");
    let visibility = VisibilityTier::Private {
        scope_label: "preview-evaluation".into(),
    };
    blob.annotations[0].visibility = visibility.clone();
    let root = repo
        .set_context_blob(Some(&root), &target, &blob)
        .expect("private context");
    let prior = repo
        .latest_state_attachment(&head.id(), repo::StateAttachmentKind::Context)
        .expect("attachment")
        .expect("prior");
    repo.put_state_attachment(&objects::object::StateAttachment {
        state_id: head.id(),
        body: objects::object::StateAttachmentBody::Context(root),
        attribution: head.attribution,
        created_at: chrono::Utc::now(),
        supersedes: Some(prior.id()),
    })
    .expect("attachment");
    json(
        temp.path(),
        &[
            "context",
            "edit",
            annotation_id,
            "--body",
            "Updated confidential decision",
        ],
    );
    assert_eq!(annotations(temp.path())[0].visibility, visibility);
    json(
        temp.path(),
        &[
            "context",
            "supersede",
            annotation_id,
            "--body",
            "Replacement confidential decision",
        ],
    );
    let rows = annotations(temp.path());
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|row| row.visibility == visibility),
        "supersede broadened visibility: {rows:?}"
    );
}
