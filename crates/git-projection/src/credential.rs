// SPDX-License-Identifier: Apache-2.0
//! Credential configuration for embedded Sley transports.

use std::{ffi::OsStr, path::Path};

use sley::{
    GitConfig, GitError,
    plumbing::sley_remote::{CredentialHelperProvider, CredentialProvider},
};
use sley_transport::GitCredential;

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
    bare_helpers: Vec<String>,
}

impl EmbeddingSafeCredentialProvider {
    pub fn new(config: &GitConfig) -> Self {
        Self::with_search_path(config, std::env::var_os("PATH").as_deref())
    }

    fn with_search_path(config: &GitConfig, search_path: Option<&OsStr>) -> Self {
        let mut config = config.clone();
        let mut bare_helpers = Vec::new();
        normalize_bare_helpers(&mut config, search_path, &mut bare_helpers);
        Self {
            config,
            bare_helpers,
        }
    }

    fn helper_error(&self, error: GitError) -> GitError {
        let helpers = self
            .bare_helpers
            .iter()
            .map(|helper| format!("'{helper}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let subject = if helpers.is_empty() {
            "configured Git credential helper".to_string()
        } else {
            format!("configured Git credential helper {helpers}")
        };
        GitError::Command(format!("{subject} failed: {error}"))
    }
}

impl CredentialProvider for EmbeddingSafeCredentialProvider {
    fn fill(&mut self, request: GitCredential) -> Result<Option<GitCredential>, GitError> {
        #[cfg(not(unix))]
        if !self.bare_helpers.is_empty() {
            return Err(GitError::Unsupported(format!(
                "embedded external credential helpers are unavailable on this platform: {}",
                self.bare_helpers.join(", ")
            )));
        }
        CredentialHelperProvider::new(Some(&self.config))
            .fill(request)
            .map_err(|error| self.helper_error(error))
    }

    fn approve(&mut self, credential: &GitCredential) -> Result<(), GitError> {
        CredentialHelperProvider::new(Some(&self.config)).approve(credential)
    }

    fn reject(&mut self, credential: &GitCredential) -> Result<(), GitError> {
        CredentialHelperProvider::new(Some(&self.config)).reject(credential)
    }
}

fn normalize_bare_helpers(
    config: &mut GitConfig,
    search_path: Option<&OsStr>,
    bare_helpers: &mut Vec<String>,
) {
    #[cfg(not(unix))]
    let _ = search_path;
    for section in &mut config.sections {
        if !section.name.eq_ignore_ascii_case("credential") {
            continue;
        }
        for entry in &mut section.entries {
            if !entry.key.eq_ignore_ascii_case("helper") {
                continue;
            }
            let Some(spec) = entry.value.as_deref().map(str::trim) else {
                continue;
            };
            if spec.is_empty() || spec.starts_with('!') {
                continue;
            }
            let (head, args) = spec
                .split_once(char::is_whitespace)
                .map_or((spec, ""), |(head, args)| (head, args.trim_start()));
            if Path::new(head).is_absolute() {
                continue;
            }
            if matches!(head, "store" | "cache" | "cache--daemon") {
                continue;
            }
            #[cfg(not(unix))]
            {
                let _ = args;
                bare_helpers.push(head.to_string());
                continue;
            }
            #[cfg(unix)]
            {
                bare_helpers.push(head.to_string());
                let executable = if head.starts_with("git-credential-") {
                    head.to_string()
                } else {
                    format!("git-credential-{head}")
                };
                let command = find_executable(&executable, search_path)
                    .map(|path| shell_quote(&path.to_string_lossy()))
                    .unwrap_or(executable);
                entry.value = Some(if args.is_empty() {
                    format!("!{command}")
                } else {
                    format!("!{command} {args}")
                });
            }
        }
    }
}

#[cfg(unix)]
fn find_executable(name: &str, search_path: Option<&OsStr>) -> Option<std::path::PathBuf> {
    std::env::split_paths(search_path?).find_map(|directory| {
        let candidate = directory.join(name);
        executable_file(&candidate).then_some(candidate)
    })
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use sley::plumbing::sley_remote::{CredentialProvider as _, http_send_with_auth};
    use sley_transport::{GitCredential, HttpResponse, parse_remote_url};
    use tempfile::TempDir;

    use super::EmbeddingSafeCredentialProvider;

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
    fn missing_bare_helper_reports_the_configured_helper() {
        let temp = TempDir::new().expect("tempdir");
        let config =
            sley::GitConfig::parse(b"[credential]\n\thelper = definitely-missing-heddle-helper\n")
                .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path(
            &config,
            Some(temp.path().as_os_str()),
        );
        let error = provider
            .fill(GitCredential {
                protocol: Some("https".to_string()),
                host: Some("example.test".to_string()),
                ..GitCredential::default()
            })
            .expect_err("missing helper must fail");

        let message = error.to_string();
        assert!(message.contains("definitely-missing-heddle-helper"));
        assert!(message.contains("credential helper"));
        assert!(!message.contains("heddle credential-"));
    }

    #[test]
    fn failing_bare_helper_reports_the_configured_helper() {
        let temp = TempDir::new().expect("tempdir");
        let helper = temp.path().join("git-credential-heddle-fails");
        fs::write(&helper, "#!/bin/sh\nexit 23\n").expect("write helper");
        let mut permissions = fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper, permissions).expect("make helper executable");

        let config = sley::GitConfig::parse(b"[credential]\n\thelper = heddle-fails\n")
            .expect("parse config");
        let mut provider = EmbeddingSafeCredentialProvider::with_search_path(
            &config,
            Some(temp.path().as_os_str()),
        );
        let error = provider
            .fill(GitCredential {
                protocol: Some("https".to_string()),
                host: Some("example.test".to_string()),
                ..GitCredential::default()
            })
            .expect_err("failing helper must fail");

        let message = error.to_string();
        assert!(message.contains("heddle-fails"));
        assert!(message.contains("credential helper"));
        assert!(!message.contains("heddle credential-"));
    }

    #[test]
    fn built_in_helpers_stay_on_sleys_embedding_dispatch() {
        let config = sley::GitConfig::parse(
            b"[credential]\n\thelper = store --file=/tmp/heddle-credentials\n",
        )
        .expect("parse config");
        let provider = EmbeddingSafeCredentialProvider::with_search_path(&config, None);

        assert_eq!(
            provider.config.get("credential", None, "helper"),
            Some("store --file=/tmp/heddle-credentials")
        );
        assert!(provider.bare_helpers.is_empty());
    }
}
