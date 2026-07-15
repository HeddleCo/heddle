// SPDX-License-Identifier: Apache-2.0
//! Combined status verdict and coordination-axis provenance.
//!
//! `StatusReport` carries two axes:
//! - **health** (`thread_health`): local checkout / verification state
//! - **coordination** (`coordination_status`): inter-thread integration state
//!
//! The builder may re-encode a health blocker onto the coordination axis as a
//! trust-derived `Blocked` only when the pre-override coordination axis was
//! genuinely clean. Presentation and any second consumer must share these pure
//! helpers so a health WIP mask never hides a genuine coordination block.

use super::CoordinationStatus;

/// Human-facing combined top-line verdict for long status text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCombinedVerdict {
    /// Verdict word (humanized health or coordination label).
    pub word: String,
    /// Optional reason naming which axis (or both) needs attention.
    pub reason: Option<&'static str>,
}

/// Combine the PRE-override (genuine, from `build_thread_view`) coordination
/// status with the trust/health override. The override may re-encode a
/// dirty / uncaptured / unverified-trust *health* blocker onto the
/// coordination axis (as a maskable trust-derived `Blocked`) ONLY when the
/// pre-override axis was genuinely CLEAN. Any genuine non-clean state from
/// `build_thread_view` — `Blocked`, `Diverged`, `Ahead`, `MergeReady`, or a
/// variant added later — wins: it is preserved as-is and stays surfaceable
/// even when the worktree is also dirty, so the health blocker never hides
/// the real coordination state.
///
/// Returns the final `coordination_status` and `coordination_blocked_by_trust`
/// — the latter true ONLY when the resulting `Blocked`'s sole source is the
/// trust/health path (i.e. a genuinely-clean axis was re-encoded). Keying the
/// whole rule on the single "was the pre-override axis genuinely clean?"
/// predicate — derived from [`coordination_axis_clean`], an exhaustive match,
/// NOT a hardcoded state list — is what closes the masking class: a new
/// `CoordinationStatus` variant is covered automatically and can never be
/// silently re-stamped as trust-derived.
pub fn resolve_coordination_with_trust(
    pre_override: CoordinationStatus,
    blocked_by_trust: bool,
    needs_checkpoint: bool,
) -> (CoordinationStatus, bool) {
    // The pre-override status comes straight from `build_thread_view`, so it
    // carries no trust encoding — read its genuine cleanliness with
    // `blocked_by_trust = false`.
    let pre_override_clean = coordination_axis_clean(&pre_override, false);
    let trust_override = blocked_by_trust && !needs_checkpoint;
    // Re-encode (and mark maskable) ONLY a genuinely-clean axis; a genuine
    // non-clean state is preserved and never marked trust-only.
    let mask_as_trust = trust_override && pre_override_clean;
    let coordination_status = if mask_as_trust {
        CoordinationStatus::Blocked
    } else {
        pre_override
    };
    (coordination_status, mask_as_trust)
}

/// Single source of truth for "is the coordination axis genuinely
/// (non-trust) clean?". `coordination_status` is overloaded: the status
/// builder re-encodes a dirty / uncaptured / unverified-trust *health*
/// blocker by forcing `coordination_status = Blocked` and carrying
/// `blocked_by_trust = true`. That Blocked is a health signal, so the
/// coordination axis is effectively clean — the health axis owns the
/// blocker. A genuine inter-thread Blocked from `build_thread_view`
/// (`ThreadState::Blocked` or concurrent actives) carries
/// `blocked_by_trust = false` and is NEVER masked, even when the worktree
/// is also dirty — a real inter-thread block can co-exist with local WIP
/// and must still surface. Keying on the Blocked's *provenance* (this
/// flag), not on `thread_health` cleanliness, is what keeps those two
/// cases apart.
pub fn coordination_axis_clean(coordination: &CoordinationStatus, blocked_by_trust: bool) -> bool {
    match coordination {
        CoordinationStatus::Clean => true,
        CoordinationStatus::Blocked => blocked_by_trust,
        CoordinationStatus::Ahead
        | CoordinationStatus::Diverged
        | CoordinationStatus::MergeReady => false,
    }
}

/// Severity rank for the `thread_health` axis. Higher = more blocking.
/// Drives which axis a non-clean combined verdict surfaces.
pub fn health_severity(thread_health: &str) -> u8 {
    match thread_health {
        "clean" => 0,
        "needs_reconcile" | "git_branch_advanced" => 4,
        "needs_init" | "needs_import" => 3,
        "needs_checkpoint" => 2,
        // dirty_worktree / uncaptured / unknown: local work in progress.
        _ => 1,
    }
}

/// Severity rank for the coordination axis. Ahead / merge-ready are
/// non-clean but benign forward states, so they rank below the
/// integration blockers (diverged / blocked).
pub fn coordination_severity(status: &CoordinationStatus) -> u8 {
    match status {
        CoordinationStatus::Clean => 0,
        CoordinationStatus::Ahead | CoordinationStatus::MergeReady => 1,
        CoordinationStatus::Diverged => 3,
        CoordinationStatus::Blocked => 4,
    }
}

/// Pure core of the combined verdict: which axes are effectively clean
/// and the resulting reason line. Split from rendering so it is
/// unit-testable without constructing a full [`super::StatusReport`].
pub fn combined_verdict_axes(
    thread_health: &str,
    coordination: &CoordinationStatus,
    coordination_blocked_by_trust: bool,
) -> (bool, bool, Option<&'static str>) {
    let health_clean = thread_health == "clean";
    let coordination_clean = coordination_axis_clean(coordination, coordination_blocked_by_trust);
    let reason = match (health_clean, coordination_clean) {
        (true, true) => None,
        (false, false) => Some("checkout health and thread coordination both need attention"),
        (false, true) => Some("checkout health needs attention"),
        (true, false) => Some("thread coordination needs attention"),
    };
    (health_clean, coordination_clean, reason)
}

/// Human-facing label for machine `thread_health` codes.
pub fn human_thread_health(status: &str) -> String {
    match status {
        "needs_init" => "setup needed".to_string(),
        "needs_import" => "setup needed".to_string(),
        "git_branch_advanced" => "Git branch advanced outside Heddle".to_string(),
        "needs_reconcile" => "Git/Heddle mismatch".to_string(),
        "needs_checkpoint" => "checkpoint needed".to_string(),
        "dirty_worktree" | "uncaptured" => "work in progress".to_string(),
        other => other.to_string(),
    }
}

/// Render the coordination axis for verbose status. A trust-derived
/// `Blocked` (sole source = the health override, so [`coordination_axis_clean`]
/// reports the axis effectively clean) shows as "work in progress" — the
/// health axis owns the blocker. Every genuine coordination state renders
/// under its own name: a genuine inter-thread `Blocked` and the non-clean
/// siblings (`Diverged` / `Ahead` / `MergeReady`) are never hidden behind
/// the WIP mask.
pub fn coordination_label(coordination: &CoordinationStatus, blocked_by_trust: bool) -> String {
    if matches!(coordination, CoordinationStatus::Blocked)
        && coordination_axis_clean(coordination, blocked_by_trust)
    {
        "work in progress".to_string()
    } else {
        coordination.to_string()
    }
}

/// Combined top-line verdict for the default long view.
///
/// `clean` only when BOTH the health and coordination axes are
/// *effectively* clean; otherwise the more-severe axis is surfaced as
/// the verdict word so a reader of the default view still learns the
/// checkout is not clean. Ties favour the local health axis — that's the
/// blocker the user usually acts on first. The reason names which axis
/// (or both) is at fault; verbose then prints the per-axis detail.
pub fn status_combined_verdict(
    thread_health: &str,
    coordination: CoordinationStatus,
    coordination_blocked_by_trust: bool,
) -> StatusCombinedVerdict {
    let (health_clean, coordination_clean, reason) =
        combined_verdict_axes(thread_health, &coordination, coordination_blocked_by_trust);
    if health_clean && coordination_clean {
        return StatusCombinedVerdict {
            word: "clean".to_string(),
            reason: None,
        };
    }
    // Surface health when it's the (or the more-severe) non-clean axis,
    // and always when the coordination axis is only health-encoded — in
    // that case the health blocker is the real story.
    let surface_health = !health_clean
        && (coordination_clean
            || health_severity(thread_health) >= coordination_severity(&coordination));
    let word = if surface_health {
        human_thread_health(thread_health)
    } else {
        coordination_label(&coordination, coordination_blocked_by_trust)
    };
    StatusCombinedVerdict { word, reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `CoordinationStatus` variant, so the table below is driven from
    /// the enum rather than a hardcoded subset — a newly added variant fails
    /// to compile here until it is listed, which is what keeps the
    /// close-the-class coverage honest.
    const ALL_COORDINATION_STATES: [CoordinationStatus; 5] = [
        CoordinationStatus::Clean,
        CoordinationStatus::Ahead,
        CoordinationStatus::Diverged,
        CoordinationStatus::Blocked,
        CoordinationStatus::MergeReady,
    ];

    #[test]
    fn dirty_wip_combined_verdict_reason_is_health_only() {
        // (a) Repro: a dirty/uncaptured checkout re-encodes its health
        // blocker as a *trust-derived* `coordination_status = Blocked`
        // (`coordination_blocked_by_trust = true`). The combined verdict
        // must NOT double-count that as a coordination failure — the
        // reason is the health/WIP reason alone, and the axis masks.
        for health in ["dirty_worktree", "uncaptured"] {
            let (health_clean, coordination_clean, reason) =
                combined_verdict_axes(health, &CoordinationStatus::Blocked, true);
            assert!(!health_clean, "{health} is a non-clean health state");
            assert!(
                coordination_clean,
                "{health}'s trust-derived Blocked is a health-signal encoding → coordination effectively clean"
            );
            assert!(
                coordination_axis_clean(&CoordinationStatus::Blocked, true),
                "{health}: trust-derived Blocked must mask (work in progress)"
            );
            assert_eq!(
                reason,
                Some("checkout health needs attention"),
                "{health}: reason must be health-only, not a coordination/both warning"
            );
            let reason = reason.unwrap();
            assert!(
                !reason.contains("coordination") && !reason.contains("both need attention"),
                "{health}: reason must not mention coordination: {reason}"
            );
        }
    }

    #[test]
    fn trust_blocked_combined_verdict_reason_is_health_only() {
        // (a') The same masking covers a trust-blocked WIP checkout: its
        // Blocked is the verification health signal (trust-derived), not
        // coordination.
        let (_, coordination_clean, reason) =
            combined_verdict_axes("git_branch_advanced", &CoordinationStatus::Blocked, true);
        assert!(coordination_clean);
        assert_eq!(reason, Some("checkout health needs attention"));
    }

    #[test]
    fn genuine_blocked_surfaces_even_when_worktree_dirty() {
        // (b) A *genuine* inter-thread block — `build_thread_view` set
        // `coordination_status = Blocked` from `ThreadState::Blocked`,
        // carrying `coordination_blocked_by_trust = false` — can co-exist
        // with local WIP. The provenance-keyed mask must NOT mask it: a
        // non-trust Blocked is always a genuine coordination block.
        for health in ["dirty_worktree", "uncaptured"] {
            assert!(
                !coordination_axis_clean(&CoordinationStatus::Blocked, false),
                "{health}: a genuine (non-trust) Blocked must never mask, even when health is dirty"
            );
            let (health_clean, coordination_clean, reason) =
                combined_verdict_axes(health, &CoordinationStatus::Blocked, false);
            assert!(!health_clean, "{health} is a non-clean health state");
            assert!(
                !coordination_clean,
                "{health}: a genuine inter-thread Blocked is a real coordination block"
            );
            assert_eq!(
                reason,
                Some("checkout health and thread coordination both need attention"),
                "{health}: the verdict reason must name the coordination block, not just health"
            );
            assert!(
                reason.unwrap().contains("coordination"),
                "{health}: reason must surface coordination: {reason:?}"
            );
        }
    }

    #[test]
    fn genuine_coordination_states_still_surface() {
        // (c) A real inter-thread coordination state with clean health
        // must still be reported by the combined verdict.
        for coordination in [
            CoordinationStatus::Ahead,
            CoordinationStatus::Diverged,
            CoordinationStatus::MergeReady,
            CoordinationStatus::Blocked,
        ] {
            assert!(
                !coordination_axis_clean(&coordination, false),
                "{coordination:?} as a genuine (non-trust) state is never clean"
            );
            let (health_clean, coordination_clean, reason) =
                combined_verdict_axes("clean", &coordination, false);
            assert!(health_clean && !coordination_clean);
            assert_eq!(
                reason,
                Some("thread coordination needs attention"),
                "{coordination:?}: combined verdict must name coordination"
            );
        }
    }

    #[test]
    fn both_axes_clean_verdict_has_no_reason() {
        // (d) All clean → clean verdict, no reason.
        let (health_clean, coordination_clean, reason) =
            combined_verdict_axes("clean", &CoordinationStatus::Clean, false);
        assert!(health_clean && coordination_clean);
        assert_eq!(reason, None);
        let verdict = status_combined_verdict("clean", CoordinationStatus::Clean, false);
        assert_eq!(verdict.word, "clean");
        assert_eq!(verdict.reason, None);
    }

    #[test]
    fn coordination_provenance_survives_trust_override_across_all_states() {
        // Close-the-class: trust/health override may re-encode a dirty /
        // unverified checkout as maskable Blocked ONLY when pre-override
        // coordination was genuinely CLEAN.
        for pre_override in ALL_COORDINATION_STATES {
            let genuinely_clean = coordination_axis_clean(&pre_override, false);
            for &trust_verified in &[true, false] {
                let blocked_by_trust = !trust_verified;
                let (coordination, blocked_by_trust_only) =
                    resolve_coordination_with_trust(pre_override, blocked_by_trust, false);
                let health = if trust_verified {
                    "clean"
                } else {
                    "dirty_worktree"
                };
                let (health_clean, coordination_clean, reason) =
                    combined_verdict_axes(health, &coordination, blocked_by_trust_only);
                let label = coordination_label(&coordination, blocked_by_trust_only);
                let ctx = format!("{pre_override:?} / trust_verified={trust_verified}");

                assert_eq!(
                    health_clean, trust_verified,
                    "{ctx}: health axis cleanliness"
                );

                if genuinely_clean {
                    assert!(
                        coordination_clean,
                        "{ctx}: a genuinely-clean axis stays effectively clean"
                    );
                    if trust_verified {
                        assert_eq!(reason, None, "{ctx}: all-clean → no reason");
                        assert_eq!(label, "clean", "{ctx}: clean axis renders as clean");
                    } else {
                        assert_eq!(
                            reason,
                            Some("checkout health needs attention"),
                            "{ctx}: clean axis + dirty worktree → health-only WIP (coordination masked)"
                        );
                        assert_eq!(
                            label, "work in progress",
                            "{ctx}: a sole-trust-derived Blocked renders as WIP, never a coordination state"
                        );
                    }
                } else {
                    assert_eq!(
                        coordination, pre_override,
                        "{ctx}: a genuine non-clean state must be preserved, not re-stamped to Blocked"
                    );
                    assert!(
                        !blocked_by_trust_only,
                        "{ctx}: a genuine non-clean axis is never marked trust-only/maskable"
                    );
                    assert!(
                        !coordination_clean,
                        "{ctx}: a genuine non-clean axis must surface, even with a dirty worktree"
                    );
                    let reason = reason.expect("a non-clean axis always yields a verdict reason");
                    assert!(
                        reason.contains("coordination"),
                        "{ctx}: the default verdict reason must name coordination: {reason}"
                    );
                    if !trust_verified {
                        assert_eq!(
                            reason, "checkout health and thread coordination both need attention",
                            "{ctx}: dirty worktree + genuine coordination state → BOTH axes surface"
                        );
                    }
                    assert_eq!(
                        label,
                        pre_override.to_string(),
                        "{ctx}: -v must show the genuine Coordination state, not the WIP mask"
                    );
                    assert_ne!(
                        label, "work in progress",
                        "{ctx}: a genuine coordination state must never be hidden behind WIP"
                    );
                }
            }
        }
    }

    #[test]
    fn needs_checkpoint_suppresses_the_trust_override() {
        // `needs_checkpoint` short-circuits the override regardless of state:
        // a genuine block is preserved, and a clean axis is left clean rather
        // than re-encoded to a trust-derived Blocked.
        let (coordination, blocked_by_trust_only) =
            resolve_coordination_with_trust(CoordinationStatus::Blocked, true, true);
        assert!(matches!(coordination, CoordinationStatus::Blocked));
        assert!(
            !blocked_by_trust_only,
            "needs_checkpoint suppresses the override; genuine block wins"
        );

        let (coordination, blocked_by_trust_only) =
            resolve_coordination_with_trust(CoordinationStatus::Clean, true, true);
        assert!(
            matches!(coordination, CoordinationStatus::Clean),
            "no override → axis stays clean"
        );
        assert!(!blocked_by_trust_only);
    }

    #[test]
    fn status_combined_verdict_surfaces_more_severe_axis() {
        let health_only = status_combined_verdict(
            "dirty_worktree",
            CoordinationStatus::Blocked,
            true, // trust-derived
        );
        assert_eq!(health_only.word, "work in progress");
        assert_eq!(health_only.reason, Some("checkout health needs attention"));

        let coord_only = status_combined_verdict("clean", CoordinationStatus::Diverged, false);
        assert_eq!(coord_only.word, "diverged");
        assert_eq!(
            coord_only.reason,
            Some("thread coordination needs attention")
        );
    }
}
