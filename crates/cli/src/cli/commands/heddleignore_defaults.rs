// SPDX-License-Identifier: Apache-2.0
//! Default `.heddleignore` template + common-noise classifier.
//!
//! The template is installed by `heddle init` on a fresh repo (no
//! `.heddleignore` present) so day-one users don't have to discover
//! the file's existence before macOS Finder writes `.DS_Store` or
//! Xcode drops `xcuserdata/` next to their source. Patterns are kept
//! conservative: only paths the entire team is overwhelmingly likely
//! to want suppressed. Anything project-specific (build outputs,
//! per-IDE preferences, `.env*`) is left to the user.
//!
//! The same per-category pattern list also backs [`noise_hint_for`],
//! which other commands (merge refusal, capture preflight) use to
//! turn a stray untracked path into a one-line suggestion.

use std::path::Path;

/// Default `.heddleignore` written by `heddle init` when no file is
/// present. Mirrors the per-category patterns enumerated in this
/// module so the two stay in sync — editing either side here also
/// edits the user-facing template.
pub const DEFAULT_HEDDLEIGNORE: &str = "\
# Default .heddleignore — installed by `heddle init`.
#
# Syntax matches `.gitignore` (globs, negation with `!`, leading `/`
# for root-anchored, trailing `/` for directory-only). See
# `docs/heddleignore.md` for the full divergence from `.gitignore` —
# in particular, heddle does NOT read `.gitignore` itself, so any
# patterns you want suppressed during `heddle capture` must live
# here.

# macOS Finder / Spotlight noise
.DS_Store
.AppleDouble
.LSOverride
Icon
._*

# Xcode / iOS dev artifacts
xcuserdata/
*.xcuserstate
*.xcscmblueprint
*.xccheckout

# JetBrains / VS Code / Fleet IDE caches
.idea/
.vscode/
.fleet/
*.iml

# Editor backups and swap files
*~
.~lock.*
*.swp
*.swo

# Windows shell metadata
Thumbs.db
desktop.ini

# OS / shell temporary debris
*.tmp

# Shell-redirect typos (`> -r foo` lands a literal file named `-r`).
# Unanchored so a typo created from a subdirectory (`src/-r`) is also
# suppressed — leading `/` would only catch the repo-root variant.
-r
-rv

# Local tool state — uncomment if your team uses these and the
# state files leak in. Left commented by default so teams that DO
# version their tool prompts (e.g. `.claude/CLAUDE.md`) aren't
# surprised by silent suppression.
# .claude/
";

/// Category of common-noise match. Used to produce both a
/// human-readable label and a suggested `.heddleignore` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseCategory {
    MacOsFinder,
    Xcode,
    IdeCache,
    EditorBackup,
    WindowsMetadata,
    TempFile,
    ShellTypo,
}

/// A suggestion to suppress one stray path. Carries enough context
/// for the caller (merge refusal, capture preflight) to render the
/// hint inline next to the offending path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoiseHint {
    pub category: NoiseCategory,
    /// One-line human label naming the noise category. Stable
    /// strings so tests can pin on them.
    pub label: &'static str,
    /// `.heddleignore` line we'd suggest adding. Either the exact
    /// basename (`Thumbs.db`) or the glob that covers the family
    /// (`*.swp`).
    pub suggested_pattern: &'static str,
}

impl NoiseHint {
    /// Pre-formatted bracket hint suitable for inline rendering:
    /// `[HINT: looks like macOS noise — add `.DS_Store` to .heddleignore?]`
    pub fn render_inline(&self) -> String {
        format!(
            "[HINT: looks like {} — add `{}` to .heddleignore?]",
            self.label, self.suggested_pattern
        )
    }
}

/// Classify `path` against the same families that the default
/// template covers. Returns `None` for paths that don't match any
/// noise category — those are presumed to be real user content the
/// caller should NOT auto-hint about.
///
/// Matching is on the path's basename (`.DS_Store`, `foo.swp`) and,
/// for directory-shaped noise like `xcuserdata/`, on any path
/// component. We deliberately don't probe the filesystem here so
/// the helper is safe to call from preflight code that may be
/// running before a tree exists on disk.
pub fn noise_hint_for(path: &Path) -> Option<NoiseHint> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // Directory-shaped patterns: any component matches.
    for comp in &components {
        match *comp {
            "xcuserdata" => {
                return Some(NoiseHint {
                    category: NoiseCategory::Xcode,
                    label: "Xcode user state",
                    suggested_pattern: "xcuserdata/",
                });
            }
            ".idea" => {
                return Some(NoiseHint {
                    category: NoiseCategory::IdeCache,
                    label: "JetBrains IDE cache",
                    suggested_pattern: ".idea/",
                });
            }
            ".vscode" => {
                return Some(NoiseHint {
                    category: NoiseCategory::IdeCache,
                    label: "VS Code workspace cache",
                    suggested_pattern: ".vscode/",
                });
            }
            ".fleet" => {
                return Some(NoiseHint {
                    category: NoiseCategory::IdeCache,
                    label: "Fleet IDE cache",
                    suggested_pattern: ".fleet/",
                });
            }
            _ => {}
        }
    }

    // Exact-basename matches.
    match name {
        ".DS_Store" => {
            return Some(NoiseHint {
                category: NoiseCategory::MacOsFinder,
                label: "macOS Finder metadata",
                suggested_pattern: ".DS_Store",
            });
        }
        ".AppleDouble" | ".LSOverride" | "Icon" => {
            return Some(NoiseHint {
                category: NoiseCategory::MacOsFinder,
                label: "macOS Finder metadata",
                suggested_pattern: name_to_static(name),
            });
        }
        "Thumbs.db" => {
            return Some(NoiseHint {
                category: NoiseCategory::WindowsMetadata,
                label: "Windows thumbnail cache",
                suggested_pattern: "Thumbs.db",
            });
        }
        "desktop.ini" => {
            return Some(NoiseHint {
                category: NoiseCategory::WindowsMetadata,
                label: "Windows folder metadata",
                suggested_pattern: "desktop.ini",
            });
        }
        "-r" | "-rv" => {
            return Some(NoiseHint {
                category: NoiseCategory::ShellTypo,
                label: "shell-redirect typo artifact",
                suggested_pattern: if name == "-r" { "-r" } else { "-rv" },
            });
        }
        _ => {}
    }

    // AppleDouble dot-underscore companions: `._foo`.
    if name.starts_with("._") && name.len() > 2 {
        return Some(NoiseHint {
            category: NoiseCategory::MacOsFinder,
            label: "macOS AppleDouble companion",
            suggested_pattern: "._*",
        });
    }

    // Extension-driven families.
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext {
            "xcuserstate" => {
                return Some(NoiseHint {
                    category: NoiseCategory::Xcode,
                    label: "Xcode user state",
                    suggested_pattern: "*.xcuserstate",
                });
            }
            "xcscmblueprint" => {
                return Some(NoiseHint {
                    category: NoiseCategory::Xcode,
                    label: "Xcode SCM blueprint",
                    suggested_pattern: "*.xcscmblueprint",
                });
            }
            "xccheckout" => {
                return Some(NoiseHint {
                    category: NoiseCategory::Xcode,
                    label: "Xcode workspace checkout",
                    suggested_pattern: "*.xccheckout",
                });
            }
            "iml" => {
                return Some(NoiseHint {
                    category: NoiseCategory::IdeCache,
                    label: "JetBrains module file",
                    suggested_pattern: "*.iml",
                });
            }
            "swp" | "swo" => {
                return Some(NoiseHint {
                    category: NoiseCategory::EditorBackup,
                    label: "Vim swap file",
                    suggested_pattern: if ext == "swp" { "*.swp" } else { "*.swo" },
                });
            }
            "tmp" => {
                return Some(NoiseHint {
                    category: NoiseCategory::TempFile,
                    label: "temporary file",
                    suggested_pattern: "*.tmp",
                });
            }
            _ => {}
        }
    }

    // Editor backup tildes (`foo.txt~`) and LibreOffice `.~lock.*`.
    if name.ends_with('~') {
        return Some(NoiseHint {
            category: NoiseCategory::EditorBackup,
            label: "Emacs/Vim backup file",
            suggested_pattern: "*~",
        });
    }
    if name.starts_with(".~lock.") {
        return Some(NoiseHint {
            category: NoiseCategory::EditorBackup,
            label: "LibreOffice lock file",
            suggested_pattern: ".~lock.*",
        });
    }

    None
}

/// Map a tiny set of literal-name noise paths to their canonical
/// `.heddleignore` line. `noise_hint_for` carries `&'static str`
/// pattern strings so the suggestion is cheap to copy out — this
/// helper exists so the match arm doesn't have to repeat the literal.
fn name_to_static(name: &str) -> &'static str {
    match name {
        ".AppleDouble" => ".AppleDouble",
        ".LSOverride" => ".LSOverride",
        "Icon" => "Icon",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn template_covers_each_category() {
        // Spot-check that every NoiseCategory has at least one
        // literal in the template — keeps the two from drifting.
        let tpl = DEFAULT_HEDDLEIGNORE;
        assert!(tpl.contains(".DS_Store"));
        assert!(tpl.contains("xcuserdata/"));
        assert!(tpl.contains(".idea/"));
        assert!(tpl.contains("*~"));
        assert!(tpl.contains("Thumbs.db"));
        assert!(tpl.contains("*.tmp"));
        // Unanchored so the typo is suppressed wherever it lands —
        // `/-r` would only catch the repo-root variant.
        assert!(tpl.contains("\n-r\n"));
        assert!(tpl.contains("\n-rv\n"));
        // Old root-anchored shape must not reappear as an actual
        // pattern line — the comment may legitimately reference it.
        assert!(!tpl.contains("\n/-r\n"));
        assert!(!tpl.contains("\n/-rv\n"));
    }

    #[test]
    fn ds_store_hint() {
        let hint = noise_hint_for(&PathBuf::from(".DS_Store")).unwrap();
        assert_eq!(hint.category, NoiseCategory::MacOsFinder);
        assert_eq!(hint.suggested_pattern, ".DS_Store");
        assert!(hint.render_inline().contains(".DS_Store"));
    }

    #[test]
    fn ds_store_in_subdir() {
        // Common shape: `src/.DS_Store`. Basename match suffices.
        let hint = noise_hint_for(&PathBuf::from("src/.DS_Store")).unwrap();
        assert_eq!(hint.category, NoiseCategory::MacOsFinder);
    }

    #[test]
    fn xcuserdata_directory_match() {
        // The match should fire on a child path under xcuserdata/,
        // not just the bare directory entry — the walker can hand
        // us either shape.
        let hint = noise_hint_for(&PathBuf::from(
            "App.xcodeproj/xcuserdata/user.xcuserdatad/UserInterfaceState.xcuserstate",
        ))
        .unwrap();
        assert_eq!(hint.category, NoiseCategory::Xcode);
        assert_eq!(hint.suggested_pattern, "xcuserdata/");
    }

    #[test]
    fn apple_double_dot_underscore() {
        let hint = noise_hint_for(&PathBuf::from("._main.rs")).unwrap();
        assert_eq!(hint.suggested_pattern, "._*");
    }

    #[test]
    fn vim_swap() {
        let hint = noise_hint_for(&PathBuf::from(".main.rs.swp")).unwrap();
        assert_eq!(hint.category, NoiseCategory::EditorBackup);
        assert_eq!(hint.suggested_pattern, "*.swp");
    }

    #[test]
    fn shell_redirect_typo() {
        let hint = noise_hint_for(&PathBuf::from("-r")).unwrap();
        assert_eq!(hint.category, NoiseCategory::ShellTypo);
        // Unanchored — leading `/` would limit suppression to the
        // repo root, but the same typo can land anywhere.
        assert_eq!(hint.suggested_pattern, "-r");
    }

    #[test]
    fn shell_redirect_typo_pattern_matches_subdir() {
        // Belt-and-braces: confirm the suggested pattern actually
        // suppresses the typo when it lands in a nested directory,
        // which the old root-anchored `/-r` would have missed.
        use objects::worktree::worktree_ignore::should_ignore;
        let patterns = vec!["-r".to_string(), "-rv".to_string()];
        assert!(should_ignore(&PathBuf::from("-r"), &patterns));
        assert!(should_ignore(&PathBuf::from("src/-r"), &patterns));
        assert!(should_ignore(&PathBuf::from("a/b/-rv"), &patterns));
    }

    #[test]
    fn libreoffice_lock() {
        let hint = noise_hint_for(&PathBuf::from(".~lock.report.odt#")).unwrap();
        assert_eq!(hint.suggested_pattern, ".~lock.*");
    }

    #[test]
    fn real_source_returns_none() {
        // The whole point: real user files must NOT trip the hint
        // detector. A false positive here would suppress real
        // content during merge or capture.
        assert!(noise_hint_for(&PathBuf::from("src/main.rs")).is_none());
        assert!(noise_hint_for(&PathBuf::from("docs/README.md")).is_none());
        assert!(noise_hint_for(&PathBuf::from("Cargo.toml")).is_none());
        // `Icon-512.png` shares a prefix with the macOS `Icon`
        // metadata sentinel but is a real app asset; exact-basename
        // match prevents the false positive.
        assert!(noise_hint_for(&PathBuf::from("assets/Icon-512.png")).is_none());
    }
}
