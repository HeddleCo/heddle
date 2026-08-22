// SPDX-License-Identifier: Apache-2.0
//! Field-study walk for heddle#1439: `heddle completions` is the public
//! script verb, first-screen help can find it, and `heddle shell
//! completion` stays intact.

use std::process::Output;

use tempfile::TempDir;

#[path = "support/mod.rs"]
mod cli_test_support;

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    cli_test_support::heddle_env(args, cwd, &[])
}

fn heddle_output(args: &[&str], cwd: Option<&std::path::Path>) -> Result<Output, String> {
    cli_test_support::heddle_output_env(args, cwd, &[])
}

/// Exact field-study command from HeddleCo/heddle#1439.
#[test]
fn field_study_1439_heddle_completions_is_recognized() {
    let output = heddle_output(&["completions"], None).expect("invoke heddle completions");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "`heddle completions` should succeed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("heddle completions bash")
            && stdout.contains("heddle completions zsh")
            && stdout.contains("heddle completions fish")
            && stdout.contains("heddle shell completion"),
        "`heddle completions` should print install lines for the supported shells: {stdout}"
    );
    assert!(
        !stderr.contains("unrecognized subcommand")
            && !stdout.contains("__complete")
            && !stderr.contains("__complete"),
        "`heddle completions` must not fall through to the hidden candidate printer: stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn first_screen_and_help_find_completions() {
    for args in [&[][..], &["help"][..], &["--help"][..]] {
        let help = heddle(args, None).unwrap_or_else(|err| {
            panic!(
                "`heddle {}` should print first-screen help: {err}",
                args.join(" ")
            )
        });
        assert!(
            help.contains("heddle completions"),
            "`heddle {}` must name completions on the ranked screen: {help}",
            args.join(" ")
        );
        assert!(
            !help.contains("heddle help advanced"),
            "the retired advanced view must not be advertised: {help}"
        );
    }

    let topic = heddle(&["help", "completions"], None)
        .expect("`heddle help completions` should render the public verb");
    assert!(
        topic.contains("Usage:") && topic.contains("completions"),
        "`heddle help completions` should render clap help for the public verb: {topic}"
    );
    assert!(
        !topic.contains("no topic or command"),
        "`heddle help completions` must not be an unknown-topic fallback: {topic}"
    );
}

#[test]
fn completions_bash_matches_shell_completion_and_leaves_complete_hidden() {
    let temp = TempDir::new().unwrap();
    let public = heddle(&["completions", "bash"], Some(temp.path()))
        .expect("`heddle completions bash` should emit a script");
    let namespaced = heddle(&["shell", "completion", "bash"], Some(temp.path()))
        .expect("`heddle shell completion bash` should keep working");

    assert!(
        public.contains("heddle") && public.contains("heddle __complete"),
        "`heddle completions bash` should emit the clap script plus dynamic helper: {public}"
    );
    assert_eq!(
        public, namespaced,
        "public `completions` and namespaced `shell completion` must emit the same script"
    );

    let complete_help = heddle(&["complete", "--help"], Some(temp.path()));
    assert!(
        complete_help.is_ok(),
        "hidden `complete` remains callable for scripts: {complete_help:?}"
    );
    let first_screen = heddle(&["help"], Some(temp.path())).expect("first-screen help");
    assert!(
        !first_screen.contains("\n  complete ") && !first_screen.contains("`heddle complete`"),
        "first-screen must not advertise the internal candidate printer: {first_screen}"
    );
}
