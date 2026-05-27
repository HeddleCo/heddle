// SPDX-License-Identifier: Apache-2.0
use objects::object::{ContentHash, FileProvenance, Origin, OriginSet};

use super::helpers::coalesce_line_spans;

#[derive(Default)]
pub(super) struct ProvenanceBuilder {
    origins: Vec<Origin>,
    origin_sets: Vec<OriginSet>,
}

impl ProvenanceBuilder {
    pub(super) fn origin_set_from_origins<I>(&mut self, origins: I) -> u32
    where
        I: IntoIterator<Item = Origin>,
    {
        let indexes = origins
            .into_iter()
            .map(|origin| self.origin_index(origin))
            .collect::<Vec<_>>();
        self.origin_set_from_indexes(indexes)
    }

    pub(super) fn origin_set_from_indexes(&mut self, mut indexes: Vec<u32>) -> u32 {
        indexes.sort_unstable();
        indexes.dedup();
        if let Some((index, _)) = self
            .origin_sets
            .iter()
            .enumerate()
            .find(|(_, set)| set.origin_indexes == indexes)
        {
            return index as u32;
        }
        let next = self.origin_sets.len() as u32;
        self.origin_sets.push(OriginSet {
            origin_indexes: indexes,
        });
        next
    }

    pub(super) fn origin_index(&mut self, origin: Origin) -> u32 {
        if let Some((index, _)) = self
            .origins
            .iter()
            .enumerate()
            .find(|(_, existing)| **existing == origin)
        {
            return index as u32;
        }
        let next = self.origins.len() as u32;
        self.origins.push(origin);
        next
    }

    pub(super) fn into_file_provenance(
        self,
        file_blob: ContentHash,
        line_count: usize,
        line_origin_sets: Vec<u32>,
    ) -> FileProvenance {
        FileProvenance::new(
            file_blob,
            line_count as u32,
            coalesce_line_spans(&line_origin_sets),
            self.origins,
            self.origin_sets,
        )
    }
}
