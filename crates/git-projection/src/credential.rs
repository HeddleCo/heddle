// SPDX-License-Identifier: Apache-2.0
//! Credential configuration for embedded Sley transports.

use std::{
    ffi::OsStr,
    io::{Cursor, Read as _},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use sley::{
    GitConfig, GitError,
    plumbing::sley_remote::{CredentialHelperProvider, CredentialProvider},
};
use sley_transport::{
    GitCredential,
    credential::{
        CredentialOpType, TIME_MAX, credential_apply_config, credential_clear_secrets,
        credential_read, credential_write,
    },
};

/// Dispatch Sley's built-in credential helpers when Heddle is the embedding
/// executable. Returns `None` for ordinary CLI arguments.
pub fn dispatch_embedded_credential_helper(args: &[String]) -> Option<Result<(), GitError>> {
    let (command, helper_args) = args.split_first()?;
    match command.as_str() {
        "credential-store" => Some(sley_transport::cmd_credential_store(helper_args)),
        "credential-cache" => Some(sley_transport::cmd_credential_cache(helper_args)),
        "credential-cache--daemon" => {
            Some(sley_transport::cmd_credential_cache_daemon(helper_args))
        }
        _ => None,
    }
}

/// Git credential helpers adapted for a process embedding Sley.
pub struct EmbeddingSafeCredentialProvider {
    config: GitConfig,
    prompt_config: GitConfig,
    has_embedded_helpers: bool,
    external_helper_config: GitConfig,
    external_helpers: Vec<ExternalCredentialHelper>,
    external_helper_timeout: Duration,
}

struct ExternalCredentialHelper {
    executable: Option<PathBuf>,
    args: Vec<String>,
}

const MAX_CREDENTIAL_HELPER_OUTPUT: usize = 64 * 1024;
const CREDENTIAL_HELPER_TIMEOUT: Duration = Duration::from_secs(30);
const EXTERNAL_HELPER_MARKER_PREFIX: &str = "heddle-external-helper-";
const EXTERNAL_HELPER_RESET_MARKER: &str = "heddle-external-helper-reset";

impl EmbeddingSafeCredentialProvider {
    pub fn new(config: &GitConfig) -> Self {
        Self::with_search_path(config, std::env::var_os("PATH").as_deref())
    }

    fn with_search_path(config: &GitConfig, search_path: Option<&OsStr>) -> Self {
        let mut config = config.clone();
        let (external_helper_config, external_helpers) =
            extract_external_helpers(&mut config, search_path);
        let has_embedded_helpers = has_credential_helpers(&config);
        let mut prompt_config = config.clone();
        remove_credential_helpers(&mut prompt_config);
        Self {
            config,
            prompt_config,
            has_embedded_helpers,
            external_helper_config,
            external_helpers,
            external_helper_timeout: CREDENTIAL_HELPER_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_search_path_and_timeout(
        config: &GitConfig,
        search_path: Option<&OsStr>,
        external_helper_timeout: Duration,
    ) -> Self {
        let mut provider = Self::with_search_path(config, search_path);
        provider.external_helper_timeout = external_helper_timeout;
        provider
    }

    fn selected_external_helpers(&self, credential: &GitCredential) -> Vec<usize> {
        let mut selection = credential.clone();
        selection.configured = false;
        selection.helpers.clear();
        if credential_apply_config(Some(&self.external_helper_config), None, &mut selection)
            .is_err()
        {
            return Vec::new();
        }
        let mut selected = Vec::new();
        for helper in selection
            .helpers
            .iter()
            .flat_map(|helpers| helpers.split_whitespace())
        {
            if helper == EXTERNAL_HELPER_RESET_MARKER {
                selected.clear();
            } else if let Some(index) = helper
                .strip_prefix(EXTERNAL_HELPER_MARKER_PREFIX)
                .and_then(|index| index.parse().ok())
            {
                selected.push(index);
            }
        }
        selected
    }

    fn run_external_helpers(
        &self,
        credential: &mut GitCredential,
        operation: &str,
        helper_indices: &[usize],
    ) {
        for &helper_index in helper_indices {
            if operation == "get" && credential.is_full() {
                break;
            }
            let Some(helper) = self.external_helpers.get(helper_index) else {
                continue;
            };
            let Some(executable) = helper.executable.as_deref() else {
                continue;
            };
            let mut command = Command::new(executable);
            command
                .args(&helper.args)
                .arg(operation)
                .stdin(Stdio::piped())
                .stdout(if operation == "get" {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stderr(Stdio::null());
            let Ok(mut child) = command.spawn() else {
                continue;
            };
            let Some(mut stdin) = child.stdin.take() else {
                terminate_helper(&mut child);
                continue;
            };
            if credential_write(credential, &mut stdin, CredentialOpType::Helper).is_err() {
                terminate_helper(&mut child);
                continue;
            }
            drop(stdin);
            let deadline = Instant::now() + self.external_helper_timeout;
            if operation != "get" {
                let _ = wait_for_helper(&mut child, deadline);
                continue;
            }
            let Some(stdout) = child.stdout.take() else {
                terminate_helper(&mut child);
                continue;
            };
            let (send_output, receive_output) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let mut output = Vec::new();
                let result = stdout
                    .take((MAX_CREDENTIAL_HELPER_OUTPUT + 1) as u64)
                    .read_to_end(&mut output)
                    .map(|_| output);
                let _ = send_output.send(result);
            });
            let output = match receive_output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Ok(output)) => output,
                Ok(Err(_)) | Err(_) => {
                    terminate_helper(&mut child);
                    continue;
                }
            };
            if output.len() > MAX_CREDENTIAL_HELPER_OUTPUT {
                terminate_helper(&mut child);
                continue;
            }
            let Some(status) = wait_for_helper(&mut child, deadline) else {
                continue;
            };
            if !status.success() {
                continue;
            }
            let mut response = credential.clone();
            if credential_read(
                &mut response,
                &mut Cursor::new(output),
                CredentialOpType::Helper,
            )
            .is_ok()
            {
                if response.password_expiry_utc < unix_time_now() {
                    credential_clear_secrets(&mut response);
                    response.password_expiry_utc = TIME_MAX;
                }
                *credential = response;
            }
        }
    }
}

fn wait_for_helper(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Err(_) => {
                terminate_helper(child);
                return None;
            }
            Ok(None) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            terminate_helper(child);
            return None;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn terminate_helper(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

impl CredentialProvider for EmbeddingSafeCredentialProvider {
    fn fill(&mut self, request: GitCredential) -> Result<Option<GitCredential>, GitError> {
        let mut credential = request;
        let external_helpers = self.selected_external_helpers(&credential);
        credential_apply_config(Some(&self.config), None, &mut credential)?;
        self.run_external_helpers(&mut credential, "get", &external_helpers);
        if credential.quit {
            return Err(GitError::InvalidFormat(
                "credential helper told Heddle to stop authentication".to_string(),
            ));
        }
        if credential.is_full() {
            return Ok(Some(credential));
        }
        credential.helpers.clear();
        match CredentialHelperProvider::new(Some(&self.config)).fill(credential.clone()) {
            Ok(result) => Ok(result),
            Err(error)
                if self.has_embedded_helpers && embedded_helper_failure_allows_prompt(&error) =>
            {
                CredentialHelperProvider::new(Some(&self.prompt_config)).fill(credential)
            }
            Err(error) => Err(error),
        }
    }

    fn approve(&mut self, credential: &GitCredential) -> Result<(), GitError> {
        let external_helpers = self.selected_external_helpers(credential);
        self.run_external_helpers(&mut credential.clone(), "store", &external_helpers);
        CredentialHelperProvider::new(Some(&self.config)).approve(credential)
    }

    fn reject(&mut self, credential: &GitCredential) -> Result<(), GitError> {
        let external_helpers = self.selected_external_helpers(credential);
        self.run_external_helpers(&mut credential.clone(), "erase", &external_helpers);
        CredentialHelperProvider::new(Some(&self.config)).reject(credential)
    }
}

fn extract_external_helpers(
    config: &mut GitConfig,
    search_path: Option<&OsStr>,
) -> (GitConfig, Vec<ExternalCredentialHelper>) {
    let mut helpers = Vec::new();
    let mut helper_config = config.clone();
    for (section, helper_section) in config.sections.iter_mut().zip(&mut helper_config.sections) {
        if !section.name.eq_ignore_ascii_case("credential") {
            helper_section.entries.clear();
            continue;
        }
        let mut helper_entries = helper_section.entries.iter_mut();
        section.entries.retain(|entry| {
            let helper_entry = helper_entries.next().expect("cloned config entry");
            if !entry.key.eq_ignore_ascii_case("helper") {
                helper_entry.key.clear();
                return true;
            }
            let spec = entry.value.as_deref().unwrap_or("").trim();
            if spec.is_empty() {
                helper_entry.value = Some(EXTERNAL_HELPER_RESET_MARKER.to_string());
                return true;
            }
            if spec.starts_with('!') {
                helper_entry.key.clear();
                return true;
            }
            let (head, args) = spec
                .split_once(char::is_whitespace)
                .map_or((spec, ""), |(head, args)| (head, args.trim_start()));
            if Path::new(head).is_absolute() {
                #[cfg(windows)]
                helpers.push(ExternalCredentialHelper {
                    executable: Some(PathBuf::from(head)),
                    args: args.split_whitespace().map(str::to_string).collect(),
                });
                #[cfg(windows)]
                {
                    helper_entry.value = Some(format!(
                        "{EXTERNAL_HELPER_MARKER_PREFIX}{}",
                        helpers.len() - 1
                    ));
                }
                #[cfg(windows)]
                return false;
                #[cfg(not(windows))]
                helper_entry.key.clear();
                #[cfg(not(windows))]
                return true;
            }
            if matches!(head, "store" | "cache" | "cache--daemon") {
                let mut helper_args = vec![format!("credential-{head}")];
                helper_args.extend(args.split_whitespace().map(str::to_string));
                helpers.push(ExternalCredentialHelper {
                    executable: std::env::current_exe().ok(),
                    args: helper_args,
                });
                helper_entry.value = Some(format!(
                    "{EXTERNAL_HELPER_MARKER_PREFIX}{}",
                    helpers.len() - 1
                ));
                return false;
            }
            let executable_name = if head.starts_with("git-credential-") {
                head.to_string()
            } else {
                format!("git-credential-{head}")
            };
            helpers.push(ExternalCredentialHelper {
                executable: find_executable(&executable_name, search_path),
                args: args.split_whitespace().map(str::to_string).collect(),
            });
            helper_entry.value = Some(format!(
                "{EXTERNAL_HELPER_MARKER_PREFIX}{}",
                helpers.len() - 1
            ));
            false
        });
        helper_section
            .entries
            .retain(|entry| entry.key.eq_ignore_ascii_case("helper"));
    }
    helper_config.sections.retain(|section| {
        section.name.eq_ignore_ascii_case("credential") && !section.entries.is_empty()
    });
    (helper_config, helpers)
}

fn has_credential_helpers(config: &GitConfig) -> bool {
    config.sections.iter().any(|section| {
        section.name.eq_ignore_ascii_case("credential")
            && section.entries.iter().any(|entry| {
                entry.key.eq_ignore_ascii_case("helper")
                    && entry
                        .value
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
            })
    })
}

fn remove_credential_helpers(config: &mut GitConfig) {
    for section in &mut config.sections {
        if section.name.eq_ignore_ascii_case("credential") {
            section
                .entries
                .retain(|entry| !entry.key.eq_ignore_ascii_case("helper"));
        }
    }
}

fn embedded_helper_failure_allows_prompt(error: &GitError) -> bool {
    match error {
        GitError::Command(message) => message.contains("credential helper"),
        #[cfg(windows)]
        GitError::Io(_) => true,
        _ => false,
    }
}

fn find_executable(name: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let names = executable_names(name);
    std::env::split_paths(search_path?).find_map(|directory| {
        names.iter().find_map(|name| {
            let candidate = directory.join(name);
            executable_file(&candidate).then_some(candidate)
        })
    })
}

fn executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        path.metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn executable_names(name: &str) -> Vec<String> {
    let names = vec![name.to_string()];
    #[cfg(windows)]
    let names = {
        let mut names = names;
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        names.extend(
            extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{name}{extension}")),
        );
        names
    };
    names
}

fn unix_time_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::Path,
        time::{Duration, Instant},
    };

    use sley::plumbing::sley_remote::{CredentialProvider as _, http_send_with_auth};
    use sley_transport::{GitCredential, HttpResponse, parse_remote_url};
    use tempfile::TempDir;

    use super::EmbeddingSafeCredentialProvider;

    fn write_helper(directory: &Path, name: &str, body: &str) {
        let helper = directory.join(format!("git-credential-{name}"));
        fs::write(&helper, format!("#!/bin/sh\n{body}\n")).expect("write helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(helper, permissions).expect("make helper executable");
    }

    fn https_credential(host: &str) -> GitCredential {
        GitCredential {
            protocol: Some("https".to_string()),
            host: Some(host.to_string()),
            ..GitCredential::default()
        }
    }

    #[test]
    fn https_transport_retries_with_bare_credential_helper_without_git_dispatch() {
        let temp = TempDir::new().expect("tempdir");
        let helper = temp.path().join("git-credential-heddle-test");
        fs::write(
            &helper,
            "#!/bin/sh\ncat >/dev/null\nprintf 'username=alice\\npassword=secret\\n'\n",
        )
        .expect("write helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions).expect("make helper executable");

        let config = sley::GitConfig::parse(b"[credential]\n\thelper = heddle-test\n")
            .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path(
            &config,
            Some(temp.path().as_os_str()),
        );
        let remote = parse_remote_url("https://example.test/repo.git").expect("HTTPS remote");
        let mut attempts = 0;
        let response = http_send_with_auth(&remote, &mut provider, |authorization| {
            attempts += 1;
            let status = if attempts == 1 {
                assert!(authorization.is_none());
                401
            } else {
                assert!(authorization.is_some_and(|value| value.starts_with("Basic ")));
                200
            };
            Ok(HttpResponse {
                status,
                content_type: None,
                body: Box::new(std::io::Cursor::new(Vec::new())),
            })
        })
        .expect("HTTPS authentication retry");

        assert_eq!(response.status, 200);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn missing_bare_helper_falls_through_to_the_next_helper() {
        let temp = TempDir::new().expect("tempdir");
        let helper = temp.path().join("git-credential-heddle-fallback");
        fs::write(
            &helper,
            "#!/bin/sh\ncat >/dev/null\nprintf 'username=fallback\npassword=secret\n'\n",
        )
        .expect("write helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions).expect("make helper executable");
        let config = sley::GitConfig::parse(
            b"[credential]\n\thelper = definitely-missing-heddle-helper\n\thelper = heddle-fallback\n",
        )
        .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path(
            &config,
            Some(temp.path().as_os_str()),
        );
        let credential = provider
            .fill(GitCredential {
                protocol: Some("https".to_string()),
                host: Some("example.test".to_string()),
                ..GitCredential::default()
            })
            .expect("fallback helper runs")
            .expect("fallback helper fills credentials");

        assert_eq!(credential.username.as_deref(), Some("fallback"));
        assert_eq!(credential.password.as_deref(), Some("secret"));
    }

    #[test]
    fn failing_bare_helper_falls_through_to_the_next_helper() {
        let temp = TempDir::new().expect("tempdir");
        let helper = temp.path().join("git-credential-heddle-fails");
        fs::write(&helper, "#!/bin/sh\nexit 23\n").expect("write helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions).expect("make helper executable");

        let fallback = temp.path().join("git-credential-heddle-good");
        fs::write(
            &fallback,
            "#!/bin/sh\ncat >/dev/null\nprintf 'username=alice\npassword=secret\n'\n",
        )
        .expect("write fallback helper");
        let mut permissions = fs::metadata(&fallback)
            .expect("fallback metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fallback, permissions).expect("make fallback executable");

        let config = sley::GitConfig::parse(
            b"[credential]\n\thelper = heddle-fails\n\thelper = heddle-good\n",
        )
        .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path(
            &config,
            Some(temp.path().as_os_str()),
        );
        let credential = provider
            .fill(GitCredential {
                protocol: Some("https".to_string()),
                host: Some("example.test".to_string()),
                ..GitCredential::default()
            })
            .expect("fallback helper runs")
            .expect("fallback helper fills credentials");

        assert_eq!(credential.username.as_deref(), Some("alice"));
        assert_eq!(credential.password.as_deref(), Some("secret"));
    }

    #[test]
    fn url_scoped_reset_runs_only_helpers_selected_for_the_request() {
        let temp = TempDir::new().expect("tempdir");
        write_helper(
            temp.path(),
            "heddle-global",
            "cat >/dev/null\nprintf 'username=global\\npassword=wrong\\n'",
        );
        write_helper(
            temp.path(),
            "heddle-other",
            "cat >/dev/null\nprintf 'username=other\\npassword=wrong\\n'",
        );
        write_helper(
            temp.path(),
            "heddle-scoped",
            "cat >/dev/null\nprintf 'username=scoped\\npassword=secret\\n'",
        );
        let config = sley::GitConfig::parse(
            b"[credential]\n\thelper = heddle-global\n\
              [credential \"https://example.test/other\"]\n\thelper = heddle-other\n\
              [credential \"https://example.test/org\"]\n\thelper =\n\thelper = heddle-scoped\n",
        )
        .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path(
            &config,
            Some(temp.path().as_os_str()),
        );

        let mut request = https_credential("example.test");
        request.path = Some("org/repo.git".to_string());
        let credential = provider
            .fill(request)
            .expect("scoped helper runs")
            .expect("scoped helper fills credentials");

        assert_eq!(credential.username.as_deref(), Some("scoped"));
        assert_eq!(credential.password.as_deref(), Some("secret"));
    }

    #[test]
    fn oversized_streaming_helper_is_killed_before_fallback_runs() {
        let temp = TempDir::new().expect("tempdir");
        write_helper(
            temp.path(),
            "heddle-oversized",
            "cat >/dev/null\nwhile :; do printf '0123456789abcdef'; done",
        );
        write_helper(
            temp.path(),
            "heddle-after-oversized",
            "cat >/dev/null\nprintf 'username=fallback\\npassword=secret\\n'",
        );
        let config = sley::GitConfig::parse(
            b"[credential]\n\thelper = heddle-oversized\n\thelper = heddle-after-oversized\n",
        )
        .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path_and_timeout(
            &config,
            Some(temp.path().as_os_str()),
            Duration::from_secs(2),
        );
        let started = Instant::now();

        let credential = provider
            .fill(https_credential("example.test"))
            .expect("oversized helper falls through")
            .expect("fallback helper fills credentials");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(credential.username.as_deref(), Some("fallback"));
    }

    #[test]
    fn nonterminating_helper_is_killed_at_the_deadline_before_fallback_runs() {
        let temp = TempDir::new().expect("tempdir");
        write_helper(temp.path(), "heddle-hangs", "cat >/dev/null\nexec sleep 30");
        write_helper(
            temp.path(),
            "heddle-after-hang",
            "cat >/dev/null\nprintf 'username=fallback\\npassword=secret\\n'",
        );
        let config = sley::GitConfig::parse(
            b"[credential]\n\thelper = heddle-hangs\n\thelper = heddle-after-hang\n",
        )
        .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path_and_timeout(
            &config,
            Some(temp.path().as_os_str()),
            Duration::from_secs(2),
        );
        let started = Instant::now();

        let credential = provider
            .fill(https_credential("example.test"))
            .expect("timed-out helper falls through")
            .expect("fallback helper fills credentials");

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(credential.username.as_deref(), Some("fallback"));
    }

    #[test]
    fn nonterminating_store_helper_is_killed_at_the_deadline() {
        let temp = TempDir::new().expect("tempdir");
        write_helper(
            temp.path(),
            "heddle-store-hangs",
            "cat >/dev/null\nexec sleep 30",
        );
        let config = sley::GitConfig::parse(b"[credential]\n\thelper = heddle-store-hangs\n")
            .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path_and_timeout(
            &config,
            Some(temp.path().as_os_str()),
            Duration::from_millis(250),
        );
        let mut credential = https_credential("example.test");
        credential.username = Some("alice".to_string());
        credential.password = Some("secret".to_string());
        let started = Instant::now();

        provider
            .approve(&credential)
            .expect("timed-out store helper is ignored");

        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn built_in_helpers_use_heddles_direct_embedding_dispatch() {
        let config = sley::GitConfig::parse(
            b"[credential]\n\thelper = store --file=/tmp/heddle-credentials\n",
        )
        .expect("parse config");
        let provider = EmbeddingSafeCredentialProvider::with_search_path(&config, None);

        assert_eq!(provider.config.get("credential", None, "helper"), None);
        assert_eq!(provider.external_helpers.len(), 1);
        assert_eq!(
            provider.external_helpers[0].args,
            [
                "credential-store".to_string(),
                "--file=/tmp/heddle-credentials".to_string()
            ]
        );
    }

    #[test]
    fn bare_manager_resolves_to_the_standalone_helper_binary() {
        let temp = TempDir::new().expect("tempdir");
        let manager = temp.path().join("git-credential-manager");
        fs::write(&manager, "#!/bin/sh\nexit 0\n").expect("write manager helper");
        let mut permissions = fs::metadata(&manager)
            .expect("manager metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&manager, permissions).expect("make manager executable");
        let config =
            sley::GitConfig::parse(b"[credential]\n\thelper = manager\n").expect("parse config");
        let provider = EmbeddingSafeCredentialProvider::with_search_path(
            &config,
            Some(temp.path().as_os_str()),
        );

        assert_eq!(provider.external_helpers.len(), 1);
        assert_eq!(
            provider.external_helpers[0].executable.as_deref(),
            Some(manager.as_path())
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::executable_names;

    #[test]
    fn manager_search_includes_windows_executable_names() {
        let names = executable_names("git-credential-manager");
        assert!(names.iter().any(|name| {
            name.eq_ignore_ascii_case("git-credential-manager.exe")
                || name.eq_ignore_ascii_case("git-credential-manager.cmd")
        }));
    }
}
