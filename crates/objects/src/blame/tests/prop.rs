// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use proptest::prelude::*;

use crate::blame::{
    BlamePreparation, BlameSliceAdvance, BlameSliceLimits, advance_file_blame_slice, blame_file,
    finalize_file_provenance, prepare_file_blame,
};
use crate::object::StateId;

use super::fixture::{put_state_with_file, store};

fn edit_strategy() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[abc\n]{0,24}", 1..8)
}

proptest! {
    #[test]
    fn oneshot_matches_repeated_slices(versions in edit_strategy()) {
        let store = store();
        let mut parent: Option<StateId> = None;
        let mut tip = None;
        for (index, body) in versions.iter().enumerate() {
            let parents = parent.map(|id| vec![id]).unwrap_or_default();
            let state = put_state_with_file(
                &store,
                "lib.rs",
                body.as_bytes(),
                parents,
                &format!("p{index}"),
            );
            parent = Some(state.id());
            tip = Some(state);
        }
        let tip = tip.expect("at least one version");
        let path = Path::new("lib.rs");
        let unlimited = BlameSliceLimits::unlimited();
        let oneshot = match blame_file(&store, &tip, path, unlimited) {
            Ok(provenance) => provenance,
            Err(_) => return Ok(()),
        };

        let limits = BlameSliceLimits {
            states: 6,
            decoded_bytes: 64 * 1024,
            lines: 256,
            diff_work: 4_096,
            scratch_bytes: 64 * 1024,
        };
        let BlamePreparation::Active {
            file_blob,
            line_count,
            mut frontier,
        } = prepare_file_blame(&store, &tip, path, limits).unwrap_or(BlamePreparation::MissingPath)
        else {
            return Ok(());
        };
        let mut finalized = Vec::new();
        for _ in 0..64 {
            match advance_file_blame_slice(&store, path, frontier, limits) {
                Ok(BlameSliceAdvance::Progress { next, finalized: more, .. }) => {
                    finalized.extend(more);
                    frontier = next;
                }
                Ok(BlameSliceAdvance::Complete { finalized: more, .. }) => {
                    finalized.extend(more);
                    break;
                }
                Err(_) => return Ok(()),
            }
        }
        let sliced = finalize_file_provenance(file_blob, line_count, finalized)
            .expect("sliced finalize");
        prop_assert_eq!(oneshot, sliced);
    }
}
