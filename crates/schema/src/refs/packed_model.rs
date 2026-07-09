// SPDX-License-Identifier: Apache-2.0
//! In-memory packed refs model and text format.

use std::collections::HashMap;

use objects::object::ChangeId;

const THREADS_PREFIX: &str = "refs/threads/";
const MARKERS_PREFIX: &str = "refs/markers/";

#[derive(Clone, Debug)]
pub struct PackedRefsModel {
    threads: HashMap<String, ChangeId>,
    markers: HashMap<String, ChangeId>,
}

impl PackedRefsModel {
    pub fn new() -> Self {
        Self {
            threads: HashMap::new(),
            markers: HashMap::new(),
        }
    }

    pub fn parse(contents: &str) -> Self {
        let mut packed = Self::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(2, ' ');
            let (Some(id_str), Some(refname)) = (parts.next(), parts.next()) else {
                continue;
            };
            let id = match ChangeId::parse(id_str) {
                Ok(id) => id,
                Err(_) => continue,
            };
            if let Some(name) = refname.strip_prefix(THREADS_PREFIX) {
                packed.threads.insert(name.to_string(), id);
            } else if let Some(name) = refname.strip_prefix(MARKERS_PREFIX) {
                packed.markers.insert(name.to_string(), id);
            }
        }
        packed
    }

    pub fn to_text(&self) -> String {
        let mut lines: Vec<String> =
            vec!["# packed-refs with: peeled fully-peeled sorted".to_string()];
        for (name, id) in &self.threads {
            lines.push(format!(
                "{} {}{}",
                id.to_string_full(),
                THREADS_PREFIX,
                name
            ));
        }
        for (name, id) in &self.markers {
            lines.push(format!(
                "{} {}{}",
                id.to_string_full(),
                MARKERS_PREFIX,
                name
            ));
        }
        lines.sort();
        lines.join("\n") + "\n"
    }

    pub fn get_thread(&self, name: &str) -> Option<ChangeId> {
        self.threads.get(name).copied()
    }
    pub fn get_marker(&self, name: &str) -> Option<ChangeId> {
        self.markers.get(name).copied()
    }
    pub fn set_thread(&mut self, name: &str, id: ChangeId) {
        self.threads.insert(name.to_string(), id);
    }
    pub fn set_marker(&mut self, name: &str, id: ChangeId) {
        self.markers.insert(name.to_string(), id);
    }
    pub fn remove_track(&mut self, name: &str) {
        self.threads.remove(name);
    }
    pub fn remove_marker(&mut self, name: &str) {
        self.markers.remove(name);
    }
    pub fn list_threads(&self) -> Vec<String> {
        self.threads.keys().cloned().collect()
    }
    pub fn list_markers(&self) -> Vec<String> {
        self.markers.keys().cloned().collect()
    }
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty() && self.markers.is_empty()
    }
}

impl Default for PackedRefsModel {
    fn default() -> Self {
        Self::new()
    }
}
