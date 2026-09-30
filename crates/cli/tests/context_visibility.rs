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
