// SPDX-License-Identifier: Apache-2.0
use crate::{
    object::{Action, ActionId, ChangeId, State, Tree},
    store::{Result, StoreError},
    util::{RetryDecision, classify_transient_io},
};

pub(super) fn validate_loaded_tree(tree: Tree) -> Result<Tree> {
    tree.validate()?;
    Ok(tree)
}

pub(super) fn validate_loaded_state(requested_id: &ChangeId, state: State) -> Result<State> {
    if state.change_id != *requested_id {
        return Err(StoreError::InvalidObject(format!(
            "state change_id mismatch: requested {}, found {}",
            requested_id, state.change_id
        )));
    }

    Ok(state)
}

pub(super) fn validate_loaded_action(requested_id: &ActionId, action: Action) -> Result<Action> {
    let found_id = action.compute_id();
    if found_id != *requested_id {
        return Err(StoreError::InvalidObject(format!(
            "action id mismatch: requested {}, found {}",
            requested_id, found_id
        )));
    }

    Ok(action)
}

pub(super) fn should_retry_store_error(error: &StoreError) -> RetryDecision {
    match error {
        StoreError::Io(error) => classify_transient_io(error),
        _ => RetryDecision::DoNotRetry,
    }
}
