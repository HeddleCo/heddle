// SPDX-License-Identifier: Apache-2.0
//! Typed source-authority decisions shared by behavior and recommendations.

use repo::{RepositorySourceAuthority, shell_quote};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAction {
    Capture,
    Commit,
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
            (RepositorySourceAuthority::GitOverlay, SourceAction::Commit) => {
                vec!["heddle", "commit", "-m", "..."]
            }
            (RepositorySourceAuthority::Native, SourceAction::Commit) => {
                vec!["heddle", "capture", "-m", "..."]
            }
            (_, SourceAction::Push) => vec!["heddle", "push"],
            (_, SourceAction::Pull) => vec!["heddle", "pull"],
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
        assert_eq!(overlay.display(SourceAction::Push), "heddle push");
        assert_eq!(overlay.display(SourceAction::Pull), "heddle pull");
        assert_eq!(native.display(SourceAction::Push), "heddle push");
        assert_eq!(native.display(SourceAction::Pull), "heddle pull");
    }

    #[test]
    fn commit_routes_to_the_authoritative_store() {
        let overlay = SourceAuthorityActions::new(RepositorySourceAuthority::GitOverlay);
        let native = SourceAuthorityActions::new(RepositorySourceAuthority::Native);
        assert_eq!(
            overlay.display(SourceAction::Commit),
            "heddle commit -m \"...\""
        );
        assert_eq!(
            native.display(SourceAction::Commit),
            "heddle capture -m \"...\""
        );
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
