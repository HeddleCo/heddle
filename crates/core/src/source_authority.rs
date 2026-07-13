// SPDX-License-Identifier: Apache-2.0
//! Typed source-authority decisions shared by behavior and recommendations.

use repo::{RepositorySourceAuthority, shell_quote};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAction {
    Capture,
    GitCommit,
    Push,
    Pull,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceAuthorityActions {
    authority: RepositorySourceAuthority,
}

impl SourceAuthorityActions {
    pub fn new(authority: RepositorySourceAuthority) -> Self {
        Self { authority }
    }

    pub fn authority(self) -> RepositorySourceAuthority {
        self.authority
    }

    pub fn argv(self, action: SourceAction) -> Vec<String> {
        match (self.authority, action) {
            (_, SourceAction::Capture) => vec!["heddle", "capture", "-m", "..."],
            (_, SourceAction::GitCommit) => vec!["git", "commit", "-m", "..."],
            (RepositorySourceAuthority::Native, SourceAction::Push) => vec!["heddle", "push"],
            (RepositorySourceAuthority::Native, SourceAction::Pull) => vec!["heddle", "pull"],
            (RepositorySourceAuthority::GitOverlay, SourceAction::Push) => vec!["git", "push"],
            (RepositorySourceAuthority::GitOverlay, SourceAction::Pull) => vec!["git", "pull"],
        }
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    pub fn display(self, action: SourceAction) -> String {
        self.argv(action)
            .into_iter()
            .map(|arg| {
                if arg == "..." {
                    "\"...\"".to_string()
                } else {
                    shell_quote(&arg)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_selects_the_source_transport() {
        let overlay = SourceAuthorityActions::new(RepositorySourceAuthority::GitOverlay);
        let native = SourceAuthorityActions::new(RepositorySourceAuthority::Native);
        assert_eq!(overlay.display(SourceAction::Push), "git push");
        assert_eq!(overlay.display(SourceAction::Pull), "git pull");
        assert_eq!(native.display(SourceAction::Push), "heddle push");
        assert_eq!(native.display(SourceAction::Pull), "heddle pull");
    }

    #[test]
    fn capture_is_the_heddle_save_boundary() {
        for authority in [
            RepositorySourceAuthority::GitOverlay,
            RepositorySourceAuthority::Native,
        ] {
            assert_eq!(
                SourceAuthorityActions::new(authority).display(SourceAction::Capture),
                "heddle capture -m \"...\""
            );
        }
    }
}
