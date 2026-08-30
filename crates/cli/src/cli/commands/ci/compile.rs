// SPDX-License-Identifier: Apache-2.0
//! Discover an SDK authoring file and compile it into the on-disk bin + lock.
//!
//! `heddle ci run --local` is one verb: find exactly one `ci.*` source,
//! compile into `.heddle/` when needed, then run. `--config` to a `.bin`
//! still means "run this blob." Rust and Go are registered so a lone
//! `ci.rs` / `ci.go` fails closed instead of being ignored.

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use ci_config::{DEFAULT_DEFINITION_FILE, DEFAULT_LOCK_FILE};
use repo::Repository;

use crate::cli::commands::RecoveryAdvice;

/// Test/advanced override for the public JS compile CLI.
const TREADLE_COMPILE_ENV: &str = "HEDDLE_TREADLE_COMPILE";
const TREADLE_COMPILE_BIN: &str = "treadle-compile";
const JS_PACKAGE_SCRIPT: &str = "node_modules/@heddleco/api/compile-treadle.mjs";
const CI_RUN_LOCAL: &str = "heddle ci run --local";
const AUTHORING_OR_BIN_HINT: &str = "Author the pipeline as ci.ts via @heddleco/api, or commit .heddle/treadle.definition.bin and treadle.lock.json.";

const AUTHORING_FILES: &[(&str, CompileDriver)] = &[
    ("ci.ts", CompileDriver::JavaScript),
    ("ci.mts", CompileDriver::JavaScript),
    ("ci.mjs", CompileDriver::JavaScript),
    ("ci.js", CompileDriver::JavaScript),
    ("ci.cts", CompileDriver::JavaScript),
    ("ci.rs", CompileDriver::Rust),
    ("ci.go", CompileDriver::Go),
];

/// One SDK language. Adding emit later is a new match arm, not a new discovery design.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum CompileDriver {
    JavaScript,
    Rust,
    Go,
}

impl CompileDriver {
    fn display_name(self) -> &'static str {
        match self {
            Self::JavaScript => "JavaScript/TypeScript",
            Self::Rust => "Rust",
            Self::Go => "Go",
        }
    }

    fn is_shipped(self) -> bool {
        matches!(self, Self::JavaScript)
    }

    /// Source in, canonical bin + lock out under `out_dir` (default `.heddle`).
    fn compile(self, source: &Path, out_dir: &Path, repo_root: &Path) -> Result<()> {
        match self {
            Self::JavaScript => compile_javascript(source, out_dir, repo_root),
            Self::Rust => Err(anyhow!(sdk_not_shipped(
                "Rust",
                source
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("ci.rs")
            ))),
            Self::Go => Err(anyhow!(sdk_not_shipped(
                "Go",
                source
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("ci.go")
            ))),
        }
    }
}

#[derive(Debug)]
enum Discovered {
    Bin {
        path: PathBuf,
    },
    Source {
        path: PathBuf,
        driver: CompileDriver,
    },
}

/// Discover an authoring file (or an explicit bin), compile if needed, return the bin path.
pub(crate) fn prepare_definition(repo: &Repository, explicit: Option<&Path>) -> Result<PathBuf> {
    match discover(repo.root(), repo.heddle_dir(), explicit)? {
        Discovered::Bin { path } => Ok(path),
        Discovered::Source { path, driver } => {
            let out_dir = repo.heddle_dir();
            if driver.is_shipped() && !needs_compile(&path, out_dir)? {
                return Ok(out_dir.join(DEFAULT_DEFINITION_FILE));
            }
            driver.compile(&path, out_dir, repo.root())?;
            Ok(out_dir.join(DEFAULT_DEFINITION_FILE))
        }
    }
}

fn discover(repo_root: &Path, heddle_dir: &Path, explicit: Option<&Path>) -> Result<Discovered> {
    let mut found = Vec::new();
    if let Some(path) = explicit {
        match driver_for_path(path) {
            None => {
                return Ok(Discovered::Bin {
                    path: path.to_path_buf(),
                });
            }
            Some(driver) => {
                if !path.is_file() {
                    return Err(anyhow!(compile_advice(
                        "ci_authoring_missing",
                        format!("CI authoring file {} does not exist", path.display()),
                        AUTHORING_OR_BIN_HINT,
                    )));
                }
                found.push((resolved_path(path)?, driver));
            }
        }
    }
    for (path, driver) in scan_repo_root(repo_root)? {
        if !found
            .iter()
            .any(|(existing, _)| paths_equal(existing, &path))
        {
            found.push((path, driver));
        }
    }

    match found.len() {
        0 => Ok(Discovered::Bin {
            path: heddle_dir.join(DEFAULT_DEFINITION_FILE),
        }),
        1 => {
            let (path, driver) = found.swap_remove(0);
            Ok(Discovered::Source { path, driver })
        }
        _ => Err(anyhow!(ambiguous_authoring(&found))),
    }
}

fn scan_repo_root(repo_root: &Path) -> Result<Vec<(PathBuf, CompileDriver)>> {
    let mut found = Vec::new();
    for (name, driver) in AUTHORING_FILES {
        let path = repo_root.join(name);
        if path.is_file() {
            found.push((resolved_path(&path)?, *driver));
        }
    }
    Ok(found)
}

fn driver_for_path(path: &Path) -> Option<CompileDriver> {
    let name = path.file_name()?.to_str()?;
    AUTHORING_FILES
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, driver)| *driver)
}

fn needs_compile(source: &Path, out_dir: &Path) -> Result<bool> {
    let bin = out_dir.join(DEFAULT_DEFINITION_FILE);
    let lock = out_dir.join(DEFAULT_LOCK_FILE);
    if !bin.is_file() || !lock.is_file() {
        return Ok(true);
    }
    let source_mtime = mtime(source)?;
    let bin_mtime = mtime(&bin)?;
    Ok(bin_mtime < source_mtime)
}

fn mtime(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .with_context(|| format!("read mtime {}", path.display()))
}

fn compile_javascript(source: &Path, out_dir: &Path, repo_root: &Path) -> Result<()> {
    let compiler = resolve_javascript_compiler(
        repo_root,
        std::env::var_os(TREADLE_COMPILE_ENV),
        std::env::var_os("PATH"),
    )?;
    invoke_javascript(&compiler, source, out_dir)
}

fn invoke_javascript(compiler: &JsCompiler, source: &Path, out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("create compile output dir {}", out_dir.display()))?;
    let mut command = match compiler {
        JsCompiler::Direct(path) => Command::new(path),
        JsCompiler::NodeScript(script) => {
            let mut command = Command::new("node");
            command.arg(script);
            command
        }
    };
    let output = command
        .arg(source)
        .arg("--out-dir")
        .arg(out_dir)
        .output()
        .with_context(|| {
            format!(
                "invoke {} to compile {}",
                compiler.label(),
                source.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("compile exited nonzero");
        return Err(anyhow!(compile_advice(
            "ci_compile_failed",
            format!("treadle-compile failed for {}: {detail}", source.display()),
            AUTHORING_OR_BIN_HINT,
        )));
    }
    let bin = out_dir.join(DEFAULT_DEFINITION_FILE);
    let lock = out_dir.join(DEFAULT_LOCK_FILE);
    if !bin.is_file() || !lock.is_file() {
        return Err(anyhow!(compile_advice(
            "ci_compile_failed",
            format!(
                "treadle-compile did not write {} and {}",
                DEFAULT_DEFINITION_FILE, DEFAULT_LOCK_FILE
            ),
            AUTHORING_OR_BIN_HINT,
        )));
    }
    Ok(())
}

#[derive(Debug)]
enum JsCompiler {
    Direct(PathBuf),
    NodeScript(PathBuf),
}

impl JsCompiler {
    fn label(&self) -> String {
        match self {
            Self::Direct(path) => path.display().to_string(),
            Self::NodeScript(script) => format!("node {}", script.display()),
        }
    }
}

fn resolve_javascript_compiler(
    repo_root: &Path,
    override_bin: Option<OsString>,
    path_var: Option<OsString>,
) -> Result<JsCompiler> {
    if let Some(value) = override_bin
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        if !path.is_file() {
            return Err(anyhow!(compile_advice(
                "ci_compile_unavailable",
                format!(
                    "{TREADLE_COMPILE_ENV} is set to {} but that file does not exist",
                    path.display()
                ),
                AUTHORING_OR_BIN_HINT,
            )));
        }
        return Ok(JsCompiler::Direct(path));
    }
    if let Some(path) = find_on_path(TREADLE_COMPILE_BIN, path_var.as_deref()) {
        return Ok(JsCompiler::Direct(path));
    }
    let script = repo_root.join(JS_PACKAGE_SCRIPT);
    if script.is_file() {
        if find_on_path("node", path_var.as_deref()).is_none() {
            return Err(anyhow!(compile_advice(
                "ci_compile_unavailable",
                "cannot compile a JavaScript/TypeScript CI file: node is not on PATH",
                AUTHORING_OR_BIN_HINT,
            )));
        }
        return Ok(JsCompiler::NodeScript(script));
    }
    Err(anyhow!(compile_advice(
        "ci_compile_unavailable",
        "cannot compile a JavaScript/TypeScript CI file: treadle-compile is not on PATH and @heddleco/api is not installed in this repo",
        AUTHORING_OR_BIN_HINT,
    )))
}

fn find_on_path(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn resolved_path(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Ok(canonical);
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("resolve CI authoring path against the current directory")?
        .join(path))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

fn ambiguous_authoring(found: &[(PathBuf, CompileDriver)]) -> RecoveryAdvice {
    let languages: BTreeSet<CompileDriver> = found.iter().map(|(_, driver)| *driver).collect();
    let listed = found
        .iter()
        .map(|(path, driver)| {
            format!(
                "{} ({})",
                path.file_name().and_then(OsStr::to_str).unwrap_or("ci.*"),
                driver.display_name()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if languages.len() > 1 {
        compile_advice(
            "ci_authoring_ambiguous",
            format!("found CI authoring files in more than one language: {listed}"),
            AUTHORING_OR_BIN_HINT,
        )
    } else {
        compile_advice(
            "ci_authoring_ambiguous",
            format!("found more than one CI authoring file: {listed}; keep exactly one"),
            AUTHORING_OR_BIN_HINT,
        )
    }
}

fn sdk_not_shipped(language: &str, file_name: &str) -> RecoveryAdvice {
    compile_advice(
        "ci_sdk_not_shipped",
        format!("the {language} CI SDK is not shipped yet (found {file_name})"),
        AUTHORING_OR_BIN_HINT,
    )
}

fn compile_advice(kind: &'static str, error: impl Into<String>, hint: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        kind,
        error,
        hint,
        "local CI cannot compile or select a single SDK authoring file",
        "running would execute a pipeline this checkout did not compile or admit",
        "the working tree and any existing .heddle/treadle.definition.bin were left unchanged",
        CI_RUN_LOCAL,
        vec![CI_RUN_LOCAL.to_string()],
    )
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, body).expect("write");
    }

    fn stub_compile(dir: &Path) -> PathBuf {
        let stub = dir.join("treadle-compile");
        write(
            &stub,
            concat!(
                "#!/bin/sh\n",
                "set -eu\n",
                "out_dir=\"\"\n",
                "while [ $# -gt 0 ]; do\n",
                "  case \"$1\" in\n",
                "    --out-dir) out_dir=\"$2\"; shift 2 ;;\n",
                "    *) shift ;;\n",
                "  esac\n",
                "done\n",
                "mkdir -p \"$out_dir\"\n",
                "printf bin > \"$out_dir/treadle.definition.bin\"\n",
                "printf lock > \"$out_dir/treadle.lock.json\"\n",
            ),
        );
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod");
        stub
    }

    #[test]
    fn discover_one_js_source_at_repo_root() {
        let root = tempfile::tempdir().expect("root");
        let heddle = root.path().join(".heddle");
        write(&root.path().join("ci.mjs"), "export {}");
        let discovered = discover(root.path(), &heddle, None).expect("discover");
        match discovered {
            Discovered::Source { path, driver } => {
                assert_eq!(driver, CompileDriver::JavaScript);
                assert_eq!(path.file_name().and_then(OsStr::to_str), Some("ci.mjs"));
            }
            Discovered::Bin { path } => panic!("expected source, got {}", path.display()),
        }
    }

    #[test]
    fn two_languages_fail_closed() {
        let root = tempfile::tempdir().expect("root");
        let heddle = root.path().join(".heddle");
        write(&root.path().join("ci.ts"), "export {}");
        write(&root.path().join("ci.rs"), "fn main() {}");
        let error = discover(root.path(), &heddle, None).expect_err("two languages");
        let advice = error
            .downcast_ref::<RecoveryAdvice>()
            .expect("typed advice");
        assert_eq!(advice.kind, "ci_authoring_ambiguous");
        assert!(advice.error.contains("more than one language"));
        assert!(advice.error.contains("ci.ts"));
        assert!(advice.error.contains("ci.rs"));
        assert_eq!(advice.primary_command, CI_RUN_LOCAL);
    }

    #[test]
    fn two_javascript_files_fail_closed() {
        let root = tempfile::tempdir().expect("root");
        let heddle = root.path().join(".heddle");
        write(&root.path().join("ci.ts"), "export {}");
        write(&root.path().join("ci.mjs"), "export {}");
        let error = discover(root.path(), &heddle, None).expect_err("two js files");
        let advice = error
            .downcast_ref::<RecoveryAdvice>()
            .expect("typed advice");
        assert_eq!(advice.kind, "ci_authoring_ambiguous");
        assert!(advice.error.contains("more than one CI authoring file"));
    }

    #[test]
    fn config_bin_skips_authoring_files() {
        let root = tempfile::tempdir().expect("root");
        let heddle = root.path().join(".heddle");
        write(&root.path().join("ci.rs"), "fn main() {}");
        let bin = heddle.join(DEFAULT_DEFINITION_FILE);
        write(&bin, "blob");
        let discovered = discover(root.path(), &heddle, Some(&bin)).expect("bin wins");
        match discovered {
            Discovered::Bin { path } => assert_eq!(path, bin),
            Discovered::Source { .. } => panic!("--config to a .bin must not compile"),
        }
    }

    #[test]
    fn config_source_compiles_that_file() {
        let root = tempfile::tempdir().expect("root");
        let heddle = root.path().join(".heddle");
        let source = root.path().join("nested").join("ci.ts");
        write(&source, "export {}");
        let discovered = discover(root.path(), &heddle, Some(&source)).expect("source");
        match discovered {
            Discovered::Source { path, driver } => {
                assert_eq!(driver, CompileDriver::JavaScript);
                assert_eq!(path, fs::canonicalize(&source).expect("canon"));
            }
            Discovered::Bin { .. } => panic!("--config to ci.ts must compile"),
        }
    }

    #[test]
    fn config_source_plus_other_language_at_root_fails() {
        let root = tempfile::tempdir().expect("root");
        let heddle = root.path().join(".heddle");
        write(&root.path().join("ci.rs"), "fn main() {}");
        let source = root.path().join("nested").join("ci.ts");
        write(&source, "export {}");
        let error = discover(root.path(), &heddle, Some(&source)).expect_err("two languages");
        let advice = error
            .downcast_ref::<RecoveryAdvice>()
            .expect("typed advice");
        assert_eq!(advice.kind, "ci_authoring_ambiguous");
        assert!(advice.error.contains("more than one language"));
    }

    #[test]
    fn no_source_uses_the_heddle_bin() {
        let root = tempfile::tempdir().expect("root");
        let heddle = root.path().join(".heddle");
        let discovered = discover(root.path(), &heddle, None).expect("default bin");
        match discovered {
            Discovered::Bin { path } => assert_eq!(path, heddle.join(DEFAULT_DEFINITION_FILE)),
            Discovered::Source { .. } => panic!("no source should keep today's bin path"),
        }
    }

    #[test]
    fn rust_driver_is_registered_and_not_shipped() {
        let error = CompileDriver::Rust
            .compile(Path::new("ci.rs"), Path::new(".heddle"), Path::new("."))
            .expect_err("rust not shipped");
        let advice = error
            .downcast_ref::<RecoveryAdvice>()
            .expect("typed advice");
        assert_eq!(advice.kind, "ci_sdk_not_shipped");
        assert!(advice.error.contains("Rust"));
        assert!(advice.error.contains("ci.rs"));
        assert!(advice.hint.contains("@heddleco/api"));
        assert!(advice.hint.contains("treadle.definition.bin"));
    }

    #[test]
    fn go_driver_is_registered_and_not_shipped() {
        let error = CompileDriver::Go
            .compile(Path::new("ci.go"), Path::new(".heddle"), Path::new("."))
            .expect_err("go not shipped");
        let advice = error
            .downcast_ref::<RecoveryAdvice>()
            .expect("typed advice");
        assert_eq!(advice.kind, "ci_sdk_not_shipped");
        assert!(advice.error.contains("Go"));
        assert!(advice.error.contains("ci.go"));
    }

    #[test]
    fn recompile_when_bin_or_lock_missing_or_stale() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("ci.mjs");
        let out_dir = root.path().join(".heddle");
        write(&source, "export {}");
        assert!(needs_compile(&source, &out_dir).expect("missing bin"));
        write(&out_dir.join(DEFAULT_DEFINITION_FILE), "bin");
        assert!(needs_compile(&source, &out_dir).expect("missing lock"));
        write(&out_dir.join(DEFAULT_LOCK_FILE), "lock");
        let bin = out_dir.join(DEFAULT_DEFINITION_FILE);
        let past = SystemTime::now()
            .checked_sub(Duration::from_secs(60))
            .expect("past");
        fs::File::open(&bin)
            .expect("open bin")
            .set_modified(past)
            .expect("age bin");
        assert!(needs_compile(&source, &out_dir).expect("stale bin"));
        let future = SystemTime::now()
            .checked_add(Duration::from_secs(60))
            .expect("future");
        fs::File::open(&bin)
            .expect("open bin")
            .set_modified(future)
            .expect("freshen bin");
        assert!(!needs_compile(&source, &out_dir).expect("fresh bin+lock"));
    }

    #[test]
    fn javascript_compiler_prefers_heddle_treadle_compile() {
        let root = tempfile::tempdir().expect("root");
        let stub = stub_compile(root.path());
        let compiler = resolve_javascript_compiler(
            root.path(),
            Some(stub.clone().into_os_string()),
            Some(OsString::from("/no-such-path")),
        )
        .expect("override");
        match compiler {
            JsCompiler::Direct(path) => assert_eq!(path, stub),
            JsCompiler::NodeScript(_) => panic!("override must win"),
        }
    }

    #[test]
    fn javascript_compiler_uses_path_then_package() {
        let root = tempfile::tempdir().expect("root");
        let path_dir = root.path().join("bin");
        let stub = stub_compile(&path_dir);
        let compiler =
            resolve_javascript_compiler(root.path(), None, Some(path_dir.clone().into_os_string()))
                .expect("PATH");
        match compiler {
            JsCompiler::Direct(path) => assert_eq!(path, stub),
            JsCompiler::NodeScript(_) => panic!("PATH must win over a missing package"),
        }

        write(&root.path().join(JS_PACKAGE_SCRIPT), "export {}");
        let node_dir = root.path().join("node-bin");
        fs::create_dir_all(&node_dir).expect("node dir");
        write(&node_dir.join("node"), "#!/bin/sh\n");
        fs::set_permissions(node_dir.join("node"), fs::Permissions::from_mode(0o755))
            .expect("chmod node");
        let compiler =
            resolve_javascript_compiler(root.path(), None, Some(node_dir.clone().into_os_string()))
                .expect("package");
        match compiler {
            JsCompiler::NodeScript(script) => {
                assert_eq!(script, root.path().join(JS_PACKAGE_SCRIPT));
            }
            JsCompiler::Direct(_) => panic!("package + node should be the last resolver"),
        }
    }

    #[test]
    fn javascript_compiler_fails_closed_when_nothing_is_installed() {
        let root = tempfile::tempdir().expect("root");
        let error =
            resolve_javascript_compiler(root.path(), None, Some(OsString::from("/no-such-path")))
                .expect_err("missing compile");
        let advice = error
            .downcast_ref::<RecoveryAdvice>()
            .expect("typed advice");
        assert_eq!(advice.kind, "ci_compile_unavailable");
        assert!(advice.error.contains("treadle-compile"));
        assert!(advice.error.contains("@heddleco/api"));
        assert_eq!(advice.primary_command, CI_RUN_LOCAL);
    }

    #[test]
    fn javascript_compile_writes_bin_and_lock() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("ci.mjs");
        write(&source, "export {}");
        let stub = stub_compile(root.path());
        let out_dir = root.path().join(".heddle");
        invoke_javascript(&JsCompiler::Direct(stub), &source, &out_dir).expect("compile");
        assert_eq!(
            fs::read_to_string(out_dir.join(DEFAULT_DEFINITION_FILE)).expect("bin"),
            "bin"
        );
        assert_eq!(
            fs::read_to_string(out_dir.join(DEFAULT_LOCK_FILE)).expect("lock"),
            "lock"
        );
    }
}
