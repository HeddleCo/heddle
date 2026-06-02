// SPDX-License-Identifier: Apache-2.0
//! Git tree-entry name classification shared by import engines.

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitTreeNameClassification {
    Representable(String),
    NeedsLossy(GitTreeNameLossy),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitTreeNameLossy {
    pub name: String,
    pub action: GitTreeNameLossyAction,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitTreeNameLossyAction {
    Dropped,
    Converted,
}

pub fn classify_git_tree_name(raw_name: &[u8]) -> GitTreeNameClassification {
    let name = match std::str::from_utf8(raw_name) {
        Ok(name) => name.to_string(),
        Err(_) => {
            return GitTreeNameClassification::NeedsLossy(GitTreeNameLossy {
                name: String::from_utf8_lossy(raw_name).into_owned(),
                action: GitTreeNameLossyAction::Converted,
                reason: "tree entry name is not valid UTF-8 and was converted with replacement characters",
            });
        }
    };

    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        return GitTreeNameClassification::NeedsLossy(GitTreeNameLossy {
            name,
            action: GitTreeNameLossyAction::Dropped,
            reason: "tree entry name is not representable in Heddle",
        });
    }

    GitTreeNameClassification::Representable(name)
}
