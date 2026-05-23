// SPDX-License-Identifier: Apache-2.0
//! Pure refspec resolution helpers.

use objects::object::ChangeId;

use super::Head;

pub fn resolve_refspec<E, ReadHead, GetThread, GetMarker>(
    refspec: &str,
    read_head: ReadHead,
    get_thread: GetThread,
    get_marker: GetMarker,
) -> Result<Option<ChangeId>, E>
where
    ReadHead: FnOnce() -> Result<Head, E>,
    GetThread: Fn(&str) -> Result<Option<ChangeId>, E>,
    GetMarker: Fn(&str) -> Result<Option<ChangeId>, E>,
{
    if refspec == "@" || refspec == "HEAD" {
        return match read_head()? {
            Head::Attached { thread } => get_thread(&thread),
            Head::Detached { state } => Ok(Some(state)),
        };
    }
    if let Some(id) = get_thread(refspec)? {
        return Ok(Some(id));
    }
    if let Some(id) = get_marker(refspec)? {
        return Ok(Some(id));
    }
    if let Ok(id) = ChangeId::parse(refspec) {
        return Ok(Some(id));
    }
    Ok(None)
}
