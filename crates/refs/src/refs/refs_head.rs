// SPDX-License-Identifier: Apache-2.0
//! HEAD reference definition and IO helpers.

use objects::{
    error::{HeddleError, Result},
    object::{StateId, ThreadName},
};

use super::{Head, RefManager, parse_state_id_text};

pub(super) struct HeadState {
    pub head: Head,
    pub exists: bool,
    pub raw: Option<String>,
}

impl RefManager {
    pub(super) fn parse_head_contents(&self, contents: &str) -> Result<Head> {
        Head::parse(contents).map_err(|error| HeddleError::InvalidObject(error.to_string()))
    }

    pub(super) fn read_head_state(&self) -> Result<HeadState> {
        let path = self.head_path();
        if !path.exists() {
            return Ok(HeadState {
                head: Head::Attached {
                    thread: ThreadName::new("main"),
                },
                exists: false,
                raw: None,
            });
        }

        let contents = self.read_string(&path)?;
        let head = self.parse_head_contents(&contents)?;
        Ok(HeadState {
            head,
            exists: true,
            raw: Some(contents),
        })
    }

    pub(super) fn read_state_id_at(
        &self,
        path: &std::path::Path,
        kind: &str,
        name: &str,
    ) -> Result<Option<StateId>> {
        let contents = match self.read_optional_string(path)? {
            Some(c) => c,
            None => return Ok(None),
        };
        match parse_state_id_text(&contents) {
            Ok(id) => Ok(Some(id)),
            Err(_) => Err(HeddleError::InvalidObject(format!(
                "invalid {} {}: {}",
                kind,
                name,
                contents.trim()
            ))),
        }
    }
}
