// SPDX-License-Identifier: Apache-2.0
//! Pure refspec resolution helpers.

use objects::object::StateId;

use super::{Head, UNDO_RECOVERY_HANDLE};

pub fn resolve_refspec<E, ReadHead, GetThread, GetMarker, GetUndoRecovery>(
    refspec: &str,
    read_head: ReadHead,
    get_thread: GetThread,
    get_marker: GetMarker,
    get_undo_recovery: GetUndoRecovery,
) -> Result<Option<StateId>, E>
where
    ReadHead: FnOnce() -> Result<Head, E>,
    GetThread: Fn(&str) -> Result<Option<StateId>, E>,
    GetMarker: Fn(&str) -> Result<Option<StateId>, E>,
    GetUndoRecovery: FnOnce() -> Result<Option<StateId>, E>,
{
    if refspec == "@" || refspec == "HEAD" {
        return match read_head()? {
            Head::Attached { thread } => get_thread(&thread),
            Head::Detached { state } => Ok(Some(state)),
        };
    }
    // Reserved heddle-internal handle, resolved BEFORE user threads/markers so
    // no user ref can intercept it (heddle#305 r3). Its leading `.` also makes
    // it uncreatable as a user ref (see `validate_ref_name`), so it is
    // unshadowable in both directions.
    if refspec == UNDO_RECOVERY_HANDLE {
        return get_undo_recovery();
    }
    if let Some(id) = get_thread(refspec)? {
        return Ok(Some(id));
    }
    if let Some(id) = get_marker(refspec)? {
        return Ok(Some(id));
    }
    if let Ok(id) = StateId::parse(refspec) {
        return Ok(Some(id));
    }
    Ok(None)
}
