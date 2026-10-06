#!/usr/bin/env python3
"""Runtime guard-removal proofs on an isolated copy of committed Heddle sources.

Every mutation must fail the named assertion, then pass after byte-exact source
restoration. Compilation errors and zero selected tests are never evidence.
Run after committing: python3 scripts/prove-native-witness-guards.py [guard ...]
"""
import argparse
import json
import os
import re
import shutil
from pathlib import Path
import subprocess
import tarfile
import tempfile
import time

NATIVE = 'crates/repo/src/thread_replication/native_witness.rs'
REPO = 'heddle-repo'
API = 'heddle-thread-api'
HOSTED = 'heddle-hosted-client'
VERIFIER = 'heddleco-capability-verifier'
CRYPTO = 'heddle-crypto'
CLI = 'heddle-cli'
TIP = 'crates/object-model/src/object/thread_replication.rs'
# name, file, exact old bytes, replacement, crate, filter, expected assertion
MUTATIONS = [
    ('native-fetch-stack', 'crates/hosted-client/src/hosted_runtime/hosted/native_provider.rs',
     '        Box::pin(async move {', '        async move {', CLI,
     'fresh_clone_capture_push_main', 'overflowed its stack'),
    ('device-publication-carrier', 'crates/hosted-client/src/hosted_runtime/device_rpc/publication.rs',
     '''                thread_api::publication::validate_source_artifacts_with_import_carriers(
                    scratch, &opening, originals, carriers,
                )?''',
     '''                thread_api::publication::validate_source_artifacts(scratch, &opening, originals)?''', HOSTED,
     'security_f2_complete_device_publication_commits_and_replays_receipt',
     'complete witnessed DeviceRpc publication'),
    ('import-tip', 'crates/object-model/src/object/thread_replication/delegated_import.rs',
     '.validate_parents_inner(genesis, parents, true)',
     '.validate_parents_inner(genesis, parents, false)', CRYPTO,
     'import_authority::tests::ancestry', 'one carrier-bound tip operation'),
    ('import-tip-strict-default', TIP,
     'self.validate_parents_inner(genesis, parents, false)',
     'self.validate_parents_inner(genesis, parents, true)', CRYPTO,
     'imported_capture_cannot_use_seed_or_a_foreign_carrier', 'carrierless and ordinary parentless Captures remain strict'),
    ('import-tip-seed', TIP, 'state.parents.contains(&genesis.base)', 'false', CRYPTO,
     'imported_capture_cannot_use_seed_or_a_foreign_carrier', 'even a genuine carrier cannot introduce the seed'),
    ('import-tip-causal-frontier', TIP, '(!imported || !self.parents.is_empty())', '!imported', CRYPTO,
     'imported_capture_with_nonempty_frontier_keeps_exact_native_ancestry', 'nonempty frontier cannot drop or invent source ancestry'),
    ('import-tip-carrier-binding', 'crates/object-model/src/object/thread_replication/delegated_import.rs',
     '''        if import_authority::frontier_digest(&expected).map_err(invalid)?
            != body.expected_frontier_digest
            || import_authority::frontier_digest(&resulting).map_err(invalid)?
                != body.resulting_frontier_digest
            || import_authority::content_digest(&content).map_err(invalid)?
                != body.resulting_content_digest''', '        if false', CRYPTO,
     'imported_capture_cannot_use_seed_or_a_foreign_carrier', "a valid imported root cannot borrow another operation's carrier"),
    ('genesis-import-policy', 'crates/repo/src/thread_replication/authority.rs',
     'if statement.policy_sequence == 0 && statement.policy_state_hash == [0; 32]',
     'if false && statement.policy_sequence == 0 && statement.policy_state_hash == [0; 32]', API,
     'published_import_at_genesis_policy_installs_on_fresh_receiver', 'authenticated genesis policy must install published import evidence'),
    ('genesis-native-policy', 'crates/repo/src/thread_replication/authority.rs',
     'if statement.policy_sequence == 0 && statement.policy_state_hash == [0; 32]',
     'if false && statement.policy_sequence == 0 && statement.policy_state_hash == [0; 32]', API,
     'native_genesis_policy_accepts_empty_revocations_on_fresh_receiver', 'genesis policy has no native revocations'),
    ('import-tip-canonical-base', TIP,
     'genesis.base != initial_base::synthetic_initial_base()?.id()', 'false', CRYPTO,
     'valid_import_carrier_cannot_unlock_noncanonical_genesis_base', 'a valid carrier must still require the canonical synthetic base'),
    ('genesis-import-zero-record', 'crates/repo/src/thread_replication/authority.rs',
     """            }) {
                return None;
            }
            return Some(&[]);""",
     """            }) {
                return Some(&[]);
            }
            return Some(&[]);""", API,
     'selected_authority_zero_policy_record_refuses_import_revocations', 'local genesis guard must reject a signed zero record for imports'),
    ('genesis-native-zero-record', 'crates/repo/src/thread_replication/authority.rs',
     """            }) {
                return None;
            }
            return Some(&[]);""",
     """            }) {
                return Some(&[]);
            }
            return Some(&[]);""", API,
     'selected_authority_zero_policy_record_refuses_native_revocations', 'local native genesis guard must reject a signed zero record'),
    ('owner-effective-interval', 'crates/repo/src/thread_replication/delegated_import.rs',
     """                from: owner.valid_from_unix_seconds().max(transfer_from),
                until: match (
                    timeline
                        .get(i + 1)
                        .map(|next| next.valid_from_unix_seconds()),
                    transfer_until,
                ) {
                    (Some(next), Some(transfer)) => Some(next.min(transfer)),
                    (next, transfer) => next.or(transfer),
                },""",
     """                from: 0,
                until: None,""", REPO,
     'owner_at_before_claim_returns_prior_authority_and_true_interval', 'assertion `left == right` failed'),
    ('owner-transfer-interval', 'crates/repo/src/thread_replication/delegated_import.rs',
     '    for record in transfers {', '    for record in transfers.iter().take(0) {', REPO,
     'owner_at_transfer_caps_prior_root_and_starts_new_owner_at_acceptance', 'prior root ends at accepted transfer'),
    ('staging-each-publication', 'crates/repo/src/thread_replication/delegated_import.rs',
     """        let digest = &operation
            .body
            .as_ref()
            .ok_or(Reject::Canonical)?
            .delegation_digest;
        let signed = bundle""",
     """        let digest = &operation
            .body
            .as_ref()
            .ok_or(Reject::Canonical)?
            .delegation_digest;
        if verified.contains_key(digest) { continue; }
        let signed = bundle""", API,
     'staging_checks_each_publication_after_job_key_revocation', "each operation must check its own publication's job revocation"),
    ('publication-needs-p3', 'crates/capability-verifier/src/import_delegation.rs',
     """    let Some(publication) = publication else {
        return Err(Error::Hybrid(contract::Reject::Transition));
    };""",
     """    let Some(publication) = publication else {
        return Ok(verified);
    };""", VERIFIER,
     'publication_admission_requires_p3_after_commit_only_preflight', 'Commit and a lone P1 must confer no genesis admission'),
    ('publication-atomic-pair', 'crates/capability-verifier/src/import_delegation.rs',
     '    contract::check_import_genesis_publication_pair(&verified.verified, s, p)?;', '', VERIFIER,
     'publication_admission_requires_equal_transaction_time_and_p1_first', 'P1/P3 must share the transaction and time with P1 first'),
    ('publication-same-executor', 'crates/capability-verifier/src/import_delegation.rs',
     '    contract::check_import_genesis_publication_pair(&verified.verified, s, p)?;', '', VERIFIER,
     'publication_admission_requires_the_same_authenticated_executor',
     'different authenticated P1/P3 executors must refuse'),
    ('host-window-ceiling', 'crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs',
     '        || prepared.response.max_validity_duration_seconds\n            > authority::MAX_DELEGATION_WINDOW_SECONDS\n', '', HOSTED,
     'alpha33_preflight_refuses_host_windows_above_seven_days',
     'even a shorter signed window cannot accept an excessive host D'),
    ('commit-conflict-wire', 'crates/hosted-client/src/hosted_runtime/hosted/import_source/job.rs',
     'detail.reason == ErrorReason::ImportDestinationConflict as i32', 'detail.reason == ErrorReason::AlreadyExists as i32', HOSTED,
     'alpha33_commit_conflicts_have_distinct_typed_outcomes',
     'Commit wire failures must preserve their typed distinction'),
    ('publication-live-p1', 'crates/capability-verifier/src/import_delegation.rs',
     """        s.observed_at_unix_millis / 1000,
        |r| is_revoked_at_accepted_order(s, r),""",
     """        signed.body.as_ref().ok_or(Error::Hybrid(contract::Reject::Canonical))?.not_before_unix_seconds,
        |r| is_revoked_at_accepted_order(s, r),""", VERIFIER,
     'publication_admission_refuses_p1_outside_delegation_window', 'P1 outside the single delegation window must refuse'),
    ('unwitnessed-capture', NATIVE, '''        if !admissions
            .get(id)
            .is_some_and(|(original, _)| original == record)
        {
            return Err(Reject::Scope.into());
        }''', '        let _ = (&admissions, id, record);', API,
     'native_requested_account_capture_requires_a_witness_payload', 'unwitnessed requested capture: native authority must reject'),
    ('unwitnessed-claim', NATIVE, '''        if !admissions
            .get(id)
            .is_some_and(|(original, _)| original == record)
        {
            return Err(Reject::Scope.into());
        }''', '        let _ = (&admissions, id, record);', API,
     'native_requested_claim_requires_its_purpose2_sidecar', 'claim without purpose 2: native authority must reject'),
    ('conflict-set', NATIVE, '''                if resolution.conflicting_claims != claims.keys().copied().collect()
                    || !claims.contains_key(&resolution.winning_claim)''', '                if false', REPO,
     'native_resolution_requires_the_complete_admitted_conflict_set', 'incomplete admitted conflict set must reject'),
    ('unresolved-conflict', NATIVE, '            None => return Err(Reject::Scope.into()),\n        };',
     '            None => claims.values().next().ok_or(Reject::Scope)?.source_frontier.iter().copied().collect(),\n        };', API,
     'native_conflicting_claims_need_a_resolution_even_with_identical_frontiers', 'unresolved same-frontier claims: native authority must reject'),
    ('unresolved-decision', NATIVE, '            None => return Err(Reject::Scope.into()),\n        };',
     '            None => claims.values().next().ok_or(Reject::Scope)?.source_frontier.iter().copied().collect(),\n        };', REPO,
     'native_unresolved_same_frontier_claims_reject', 'same-frontier ownership conflict must reject'),
    ('orphan-resolution', NATIVE, '''    if resolutions
        .keys()
        .any(|thread| !claims.contains_key(thread))''', '    if false', REPO,
     'native_resolution_without_claims_rejects', 'resolution without admitted claims must reject'),
    ('genesis-job-key', 'crates/crypto/src/native_witness.rs',
     '            .any(|(k, _)| k.as_slice() == genesis.creator)',
     '            .any(|(k, _)| k.as_slice() == genesis.creator) && false', API,
     'native_genesis_creator_cannot_be_a_job_key_or_authority_key', 'native creator key role must reject'),
    ('genesis-forbidden-key', 'crates/crypto/src/native_witness.rs',
     '        .any(|k| k.as_slice() == genesis.creator)',
     '        .any(|k| k.as_slice() == genesis.creator) && false', API,
     'native_genesis_creator_cannot_be_a_job_key_or_authority_key', 'native creator key role must reject'),
    ('binding-ambiguity', 'crates/repo/src/thread_replication/authority.rs', '''                        if resolved.is_some() {
                            return Err(Reject::Canonical.into());
                        }''', '', API,
     'native_binding_rejects_ambiguous_owner_chain_resolution', 'ambiguous owner chain must reject'),
    ('duplicate-genesis', NATIVE, 'if geneses.insert(id, (signed, envelope.to_vec())).is_some() {',
     'if geneses.insert(id, (signed, envelope.to_vec())).is_some() && false {', REPO,
     'native_duplicate_genesis_admission_rejects', 'duplicate genesis admission must reject'),
    ('native-after-import', NATIVE, '''    if imported {
        return Err(Reject::Scope.into());
    }''', '    let _ = imported;', REPO,
     'native_and_import_retention_are_exclusive_without_shared_admissions', 'cross-arm retention must reject'),
    ('import-after-native', 'crates/repo/src/thread_replication/delegated_import.rs', '''    if native {
        return Err(Reject::Scope.into());
    }''', '    let _ = native;', REPO,
     'native_and_import_retention_are_exclusive_without_shared_admissions', 'cross-arm retention must reject'),
    ('ready-binding', 'crates/thread-api/src/fetch/hosted.rs', '    if !bundle.genesis_witnesses.iter().any(|p| {',
     '    if false && !bundle.genesis_witnesses.iter().any(|p| {', API,
     'native_ready_binding_must_match_the_exact_witnessed_genesis', 'Ready original, envelope and binding must match'),
    ('carrierless-export', 'crates/thread-api/src/replication/native/hosted.rs',
     '            (None, None) => return Err(Error::HostedTrustRequired),',
     '            (None, None) => return self.local.operation(id).await,', API,
     'native_hosted_export_refuses_a_carrierless_local_original', 'carrierless hosted export must reject'),
    ('publication-cutoff', 'crates/repo/src/thread_replication/source_publication.rs',
     '        && native_local_source_covered(tx, &op)?;', '        && true;', REPO,
     'publication_settle_requires_admission_or_native_local_claim_coverage', 'a native proof row alone cannot authorize local work'),
    ('publication-import-arm', 'crates/repo/src/thread_replication/source_publication.rs',
     '    let local = native\n', '    let local = true\n', REPO,
     'publication_settle_requires_admission_or_native_local_claim_coverage', 'import arm cannot borrow native claim coverage'),
    ('typed-summary', 'crates/capability-verifier/src/native_genesis.rs',
     '        owner_kind,\n    })', '        owner_kind: NativeOwnerKind::Account,\n    })', VERIFIER,
     'native_summary_distinguishes_account_authority_from_local_binding', 'binding kind must be explicit'),
    ('premature-binding', 'crates/hosted-client/src/hosted_runtime/hosted/native_sync.rs',
     '                .stage_native_genesis_binding(&binding)', '                .retain_native_genesis_binding(&binding)', HOSTED,
     'start_thread_failure_keeps_binding_pending_and_retry_tracks_owner_rotation', 'failed StartThread must not finalize binding'),
    ('stale-pending-binding', 'crates/hosted-client/src/hosted_runtime/hosted/native_sync.rs',
     '        if creation.request().native_genesis_authority.is_none() {',
     '''        if let Some(binding) = replica.pending_native_genesis_binding().map_err(replica_err)? {
            creation = creation.with_native_authority(binding).map_err(native_error)?;
        }
        if creation.request().native_genesis_authority.is_none() {''', HOSTED,
     'start_thread_failure_keeps_binding_pending_and_retry_tracks_owner_rotation', 'retry recomputes exactly when selected owner changes'),
    ('pending-migration', 'crates/repo/src/local_metadata.rs', 'pub const SCHEMA_VERSION: i64 = 7;',
     'pub const SCHEMA_VERSION: i64 = 6;', REPO,
     'version_six_migrates_pending_native_bindings_without_touching_final_bindings', 'new pending table'),
    ('part2-install', 'crates/thread-api/src/fetch/hosted.rs',
     '        crate::hybrid::transfer_ready(&self.ready).map_err(Error::Invalid)?;',
     '''        repository.install_native_spool_id(
            self.ready.thread.as_ref().and_then(|t| t.spool.as_ref())
                .ok_or(Error::Invalid("Spool absent"))?.id.parse::<uuid::Uuid>()
                .map_err(preparation)?
        ).map_err(preparation)?;
        crate::hybrid::transfer_ready(&self.ready).map_err(Error::Invalid)?;''', API,
     'verify_before_install_rejects_without_partial_repository_mutation', 'rejection must precede Spool mutation'),
    ('part2-durable-witness', 'crates/repo/src/thread_replication/hosted_trust.rs',
     'verify_set(signed, &expected, previous.as_ref())?', 'verify_set(signed, &expected, None)?', API,
     'staged_context_rechecks_concurrent_durable_revocation_before_install',
     'staged N must never authorize install after durable N+1 revocation'),
    ('same-job-history', 'crates/repo/src/thread_replication/delegated_import.rs',
     '        for old in histories.iter().skip(1) {',
     '        for old in histories.iter().skip(histories.len()) {', REPO,
     'hybrid_job_history_advances_unselected_threads_and_rejects_every_older_projection',
     "same job must not lose another Thread's admitted originals"),
    ('same-job-projections', 'crates/repo/src/thread_replication/delegated_import.rs',
     'UPDATE hosted_import_proofs SET bundle=?3 WHERE authority=?1 AND bundle=?2',
     'UPDATE hosted_import_proofs SET bundle=?3 WHERE 0 AND authority=?1 AND bundle=?2', REPO,
     'hybrid_job_history_advances_unselected_threads_and_rejects_every_older_projection',
     'unselected Thread history advances atomically'),
    ('root-checkpoint', 'crates/repo/src/thread_replication/hosted_trust.rs',
     ' AND (signed_set IS NULL OR (root_id=history_root_id AND root_key=history_root_key))',
     '', REPO, 'cached_context_concurrent_revocation_and_root_replacement',
     'a retained checkpoint cannot skip an unadmitted root epoch'),

]

# The import composition helper also enforces the durable witness checkpoint.
# Remove both checks to prove the family; removing one leaves the other active.
EXTRA_MUTATIONS = {
    'native-fetch-stack': [
        ('crates/hosted-client/src/hosted_runtime/hosted/native_provider.rs',
         '        })\n        .await\n    }', '        }\n        .await\n    }'),
    ],
    'publication-live-p1': [
        ('crates/capability-verifier/src/import_delegation.rs',
         '    contract::check_import_genesis_publication_pair(&verified.verified, s, p)?;', ''),
    ],
    'part2-durable-witness': [
        ('crates/repo/src/thread_replication/delegated_import.rs',
         '        snapshot.as_ref(),', '        None,'),
    ],
}


# Part 8 review guards. These transformations deliberately remove real checks;
# every selected runtime assertion must fail and pass after exact restoration.
def replace_once(source, old, new):
    assert old in source, old
    return source.replace(old, new, 1)


def no_cut(source, role):
    start = source.index('permission::thread_control_authority::Revocation::' + role + '(key) => {')
    end = source.index('\n            }', start)
    body = source[start:end]
    old = '|| revoked.contains(&api::hybrid_codec::key_id(key).to_vec())'
    assert old in body
    return source[:start] + body.replace(old, '|| false') + source[end:]


def empty_fn(source, name, visibility='pub '):
    start = source.index(visibility + 'fn ' + name + '(')
    end = source.index('\n}\n', start)
    body = source[start:end]
    opening = body.index(' {\n')
    return source[:start] + body[:opening + 3] + '    Ok(())' + source[end:]


def no_review(source):
    return empty_fn(source, 'require_review_operation', '')


PART8_MUTATIONS = [
    ('foreign-fetch-before-parents', 'crates/thread-api/src/fetch/staging.rs',
     lambda s: replace_once(s, '        if foreign_endpoint(&foreign, operation, signed)? {',
        '        for parent in &operation.parents {\n            decoded.get(parent).ok_or(Error::Invalid("incomplete source ancestry"))?;\n        }\n        if foreign_endpoint(&foreign, operation, signed)? {'), API,
     'fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint'),
    ('foreign-fetch-past-endpoint', 'crates/thread-api/src/fetch/staging.rs',
     lambda s: replace_once(s, '            edges.insert(id, BTreeSet::new());\n            continue;',
        '            edges.insert(id, operation.parents.clone());\n            pending.extend(&operation.parents);\n            continue;'), API,
     'fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint'),
    ('foreign-prefix-endpoint-cut', 'crates/thread-api/src/fetch/staging.rs',
     lambda s: replace_once(s, 'if !foreign_endpoint(projected.foreign_dependencies(), op, signed)? {', 'if true {'), API,
     'fresh_fetch_native_main_with_landed_import_stops_at_exact_foreign_endpoint'),
    ('foreign-fetch-exact-digest', 'crates/thread-api/src/fetch/staging.rs',
     lambda s: replace_once(s, '&& r.signed_native_digest == digest', '&& !digest.is_empty()'), API,
     'forged_foreign_endpoint_is_traversed_and_rejected'),
    ('foreign-fetch-native-ancestry', 'crates/thread-api/src/fetch/staging.rs',
     lambda s: replace_once(s, 'None => operation.validate_parents(genesis, &parents),', 'None => Ok(()),'), API,
     'native_fetch_mismatched_ancestry_is_rejected'),
    ('foreign-fetch-claim-cutoff', 'crates/thread-api/src/fetch/staging.rs',
     lambda s: replace_once(s, 'if !claim_threads.contains(thread) {', 'if false && !claim_threads.contains(thread) {'), API,
     'source_staging_retains_signed_claim_cutoff_beyond_selected_revision'),
    ('receiver-clock-sampling-interval', 'crates/repo/src/thread_replication/hosted_trust.rs',
     lambda s: replace_once(s, '''        let before = clock.elapsed_millis()?;
        let wall = clock.now_millis()?;
        let after = clock.elapsed_millis()?;''', '''        let wall = clock.now_millis()?;
        let after = clock.elapsed_millis()?;
        let before = after;'''), REPO,
     'receiver_clock_sampling_delay_does_not_report_rollback'),
    ('receiver-clock-wall-rollback', 'crates/repo/src/thread_replication/hosted_trust.rs',
     lambda s: replace_once(s, 'if now.wall < floor {', 'if false && now.wall < floor {'), REPO,
     'receiver_clock_wall_rollback_is_refused'),
    ('receiver-clock-monotonic-regression', 'crates/repo/src/thread_replication/hosted_trust.rs',
     lambda s: replace_once(s, '''    let passed = now
        .before
        .checked_sub(last.after)
        .ok_or(Error::HostedClock)?;''', '    let passed = now.before.saturating_sub(last.after);'), REPO,
     'receiver_clock_monotonic_regression_is_refused'),
    ('receiver-clock-intrasample-regression', 'crates/repo/src/thread_replication/hosted_trust.rs',
     lambda s: replace_once(s, 'if after < before {', 'if false && after < before {'), REPO,
     'receiver_clock_monotonic_regression_within_sample_is_refused'),
    ('wasm-spool-owner-pin', 'crates/capability-verifier/src/native_genesis.rs',
     lambda s: replace_once(s, '            .account_uuid\n    {', '            .account_uuid && false\n    {'), VERIFIER,
     'native_genesis_owner_pin_cuts_truncated_pre_recover_author_history'),
    ('spool-owner-real-receiver-pin', 'crates/crypto/src/writer_authority.rs',
     lambda s: replace_once(s, '            == account\n        {', '            == account && false\n        {'), API,
     'spool_owner_pre_recover_history_real_receiver_rejects_cut_device'),
    ('foreign-fetch-depth', 'crates/hosted-client/src/hosted_runtime/hosted/native_sync.rs',
     lambda s: replace_once(s, '            budget\n                .visit(outstanding.len() + 1, false)\n                .map_err(replica_err)?;', ''), HOSTED,
     'foreign_prefix_fetch_depth_33_refuses_with_typed_limit'),
    ('foreign-replay-depth', 'crates/repo/src/thread_replication/foreign_dependencies.rs',
     lambda s: replace_once(s, '    check\n        .budget\n        .borrow_mut()\n        .visit(check.path.len() + 1, true)?;', ''), API,
     'foreign_prefix_replay_depth_33_refuses_with_typed_limit'),
    ('foreign-installed-count', 'crates/repo/src/thread_replication/foreign_dependencies.rs',
     lambda s: replace_once(s, 'visit(check.path.len() + 1, true)?', 'visit(check.path.len() + 1, false)?'), API,
     'installed_foreign_closure_larger_than_fetch_count_remains_installable'),
    ('foreign-replay-error-type', 'crates/thread-api/src/fetch/hosted.rs',
     lambda s: replace_once(s, '            .map_err(Error::from)?;\n            (replicas, Vec::new())', '            .map_err(preparation)?;\n            (replicas, Vec::new())'), API,
     'foreign_prefix_replay_depth_33_refuses_with_typed_limit'),
    ('hosted-replay-error-type', 'crates/hosted-client/src/hosted_runtime/hosted/native_sync.rs',
     lambda s: replace_once(s, '''            ProtocolError::ForeignPrefixLimitExceeded { limit_name, limit }
        }
        error => native_error(error),''', '''            native_error(format!("foreign prefix {limit_name} exceeds limit {limit}"))
        }
        error => native_error(error),'''), HOSTED,
     'hosted_replay_preserves_foreign_prefix_limit_type'),
    ('spool-owner-receiver-pin', 'crates/crypto/src/writer_authority.rs',
     lambda s: replace_once(s, '            == account\n        {', '            == account && false\n        {'), API,
     'spool_owner_pre_recover_history_cannot_revive_cut_device'),
    ('governance-is-not-author', 'crates/crypto/src/writer_authority.rs',
     lambda s: replace_once(s, '            == account\n        {', '            == account || true\n        {'), API,
     'cowriter_start_capture_own_and_owner_thread_and_review'),
    ('foreign-resolver', 'crates/repo/src/thread_replication/native_witness.rs',
     lambda s: replace_once(s, '|r| foreign.resolve(r),', '|_| Ok(None),'), API,
     'foreign_import_tip_lands_into_native_fast_forward_and_merge'),
    ('installed-status', 'crates/repo/src/thread_replication/foreign_dependencies.rs',
     lambda s: replace_once(s, 'id=?1 AND thread=?2 AND status=1', 'id=?1 AND thread=?2'), API,
     'foreign_original_requires_a_durably_installed_operation'),
    ('attachment-required', 'crates/capability-verifier/src/thread_control_authority.rs',
     lambda s: replace_once(s, 'if admitted_mint_roots.contains(attachment) {', 'if false {'), API,
     'paired_cowriter_after_rotate_uses_witness_admitted_attachment'),
    ('attachment-exact-inventory', 'crates/capability-verifier/src/thread_control_authority.rs',
     lambda s: replace_once(s, 'if admitted_mint_roots.contains(attachment) {', 'if true {'), API,
     'forged_old_owner_certificate_cannot_join_durable_attachment_inventory'),
    ('actor-publisher-cut', 'crates/repo/src/thread_replication/authority.rs',
     lambda s: no_cut(s, 'Publisher'), API,
     'receiver_policy_cuts_the_independent_actor_publisher_and_mint'),
    ('actor-mint-cut', 'crates/repo/src/thread_replication/authority.rs',
     lambda s: no_cut(s, 'MintRoot'), API,
     'receiver_policy_cuts_the_independent_actor_publisher_and_mint'),
    ('boundary-prefix-scope', 'crates/repo/src/thread_replication/prefix.rs',
     lambda s: replace_once(s, 'projected.require_boundary_originals()?;', ''), API,
     'boundary_p1_prefix_refuses_selection_beyond_cutoff'),
    ('p4-own-admission', 'crates/crypto/src/import_authority.rs',
     lambda s: s[:s.index('pub fn verify_landing_payload(')] + s[s.index('pub fn verify_landing_payload('):].replace('if !closure.foreign(', 'if false && !closure.foreign('), CRYPTO,
     'native_authority_ownership_and_landing_preserve_original_closure'),
    ('root-owner-identity', '@api/src/writer_authority.rs',
     lambda s: replace_once(s, 'if actor_account == spool_account && root.owner_id != spool_owner_id {', 'if false {'), API,
     'writer_account_and_owner_identity_negatives_reject_then_controls_pass'),
    ('requester-token-binding', '@api/src/writer_authority.rs',
     lambda s: empty_fn(s, 'verify_landing_actor_binding'), API,
     'landing_requester_must_be_the_verified_token_subject'),
    ('p2-actor-binding', '@api/src/writer_authority.rs',
     lambda s: empty_fn(s, 'verify_authority_actor_binding'), API,
     'p2_envelope_cannot_replace_the_verified_operation_author'),
    ('recover-cuts-attachments', '@api/src/writer_authority.rs',
     lambda s: replace_once(s, 'if t.kind == 2 {', 'if false {'), API,
     'paired_cowriter_after_rotate_uses_witness_admitted_attachment'),
    ('writer-key-policy', '@api/src/writer_authority.rs',
     lambda s: s.replace('return Err(Reject::Revoked);', 'return Ok(());'), API,
     'actor_key_policy_cut_and_non_review_p4_reject_then_controls_pass'),
    ('counterparty-policy', '@api/src/writer_authority.rs',
     lambda s: replace_once(s, '.any(|s| revoked.contains(&key_id(&s.public_key)))', '.any(|_| false)'), API,
     'actor_key_policy_cut_and_non_review_p4_reject_then_controls_pass'),
    ('review-only-p4', '@api/src/import_authority.rs',
     no_review, API,
     'actor_key_policy_cut_and_non_review_p4_reject_then_controls_pass'),
    ('job-request-role', '@api/src/import_authority.rs',
     lambda s: empty_fn(s, 'verify_landing_key_roles'), API,
     'job_key_cannot_sign_landing_request'),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('guards', nargs='*')
    parser.add_argument('--source', type=Path, help='Reuse an already isolated source copy')
    parser.add_argument('--output', type=Path, default=Path('/tmp/1968-review-guard-proofs'))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    sha = subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip()
    args.output.mkdir(parents=True, exist_ok=True)
    source = args.source or Path(tempfile.mkdtemp(prefix='source-', dir=args.output))
    if not args.source:
        archive = args.output / 'source.tar'
        with archive.open('wb') as output:
            subprocess.run(['git', '-C', str(root), 'archive', sha], stdout=output, check=True)
        with tarfile.open(archive) as contents:
            contents.extractall(source, filter='data')
        archive.unlink()
        for rust in source.rglob('*.rs'):
            rust.touch()
    assert source.resolve() != root.resolve(), 'Guard mutations require isolated sources'
    target = os.environ.get('NATIVE_PROOF_TARGET', os.environ.get('CARGO_TARGET_DIR', str(args.output / 'target')))
    receipts = []
    selected = [m for m in MUTATIONS if not args.guards or m[0] in args.guards]
    part8 = [m for m in PART8_MUTATIONS if not args.guards or m[0] in args.guards]
    assert (selected or part8) and set(args.guards) <= {m[0] for m in MUTATIONS + PART8_MUTATIONS}
    if any(m[1].startswith('@api/') for m in part8):
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--offline', '--locked', '--format-version', '1'], cwd=source))
        package = next(p for p in metadata['packages'] if p['name'] == 'heddle-api' and p['source'] and p['source'].startswith('git+'))
        api_source = args.output.resolve() / 'api-source'
        assert not api_source.exists(), 'Use a fresh output directory for isolated API mutations'
        shutil.copytree(Path(package['manifest_path']).parent, api_source,
                        ignore=shutil.ignore_patterns('.git', 'target', 'node_modules'))
        manifest = source / 'Cargo.toml'
        contents = manifest.read_text()
        pattern = r'git = "https://github.com/HeddleCo/api", rev = "[0-9a-f]{40}", version = "(=[^"]+)"'
        contents, count = re.subn(pattern, lambda m: 'path = "' + str(api_source) + '", version = "' + m[1] + '"', contents)
        assert count == 2, 'Both API entries must relocate together'
        manifest.write_text(contents)
        subprocess.run(['cargo', 'update', '--offline', '-p', 'heddle-api'], cwd=source, check=True)
    for name, file, mutate, crate, test in part8:
        path = api_source / file.removeprefix('@api/') if file.startswith('@api/') else source / file
        original = path.read_text()
        changed = mutate(original)
        assert changed != original, name
        # Whole-file replacement permits intentional multi-site guard removals.
        selected.append((name, str(path), original, changed, crate, test, test))
    print('SOURCE', source, 'SHA', sha, flush=True)
    for name, file, old, new, crate, test, assertion in selected:
        path = source / file
        original = path.read_text()
        assert original.count(old) == 1, (name, original.count(old))
        changes = [(path, original, old, new)]
        for file, before, after in EXTRA_MUTATIONS.get(name, []):
            extra_path = source / file
            extra_original = extra_path.read_text()
            assert extra_original.count(before) == 1, (name, file, extra_original.count(before))
            changes.append((extra_path, extra_original, before, after))
        for red in [True, False]:
            label = name + ('-red' if red else '-green')
            command = ['cargo', 'test', '--offline', '--locked', '-p', crate]
            if crate == CLI:
                command += ['--features', 'ci', '--test', 'hosted_clone_writes']
            else:
                command += ['--lib']
            if crate == HOSTED:
                command += ['--features', 'client']
            elif crate == CRYPTO:
                command += ['--features', 'owner-root']
            if name == 'wasm-spool-owner-pin':
                command += ['--target', 'wasm32-unknown-unknown']
            command += [test, '--', '--nocapture']
            if name != 'wasm-spool-owner-pin':
                command += ['--test-threads', '1']
            log = args.output / (label + '.log')
            env = {**os.environ, 'CARGO_TARGET_DIR': target,
                   'HEDDLE_HOME': tempfile.mkdtemp(prefix='home-', dir=args.output)}
            if name == 'wasm-spool-owner-pin':
                env['CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER'] = 'wasm-bindgen-test-runner'
            start = time.monotonic()
            print('RUN', label, flush=True)
            try:
                if red:
                    edited = {}
                    for changed_path, contents, before, after in changes:
                        contents = edited.get(changed_path, contents)
                        assert contents.count(before) == 1, (name, changed_path)
                        edited[changed_path] = contents.replace(before, after)
                    for changed_path, contents in edited.items():
                        changed_path.write_text(contents)
                with log.open('w') as output:
                    result = subprocess.run(command, cwd=source, env=env, stdout=output, stderr=subprocess.STDOUT)
            finally:
                for changed_path, contents, _, _ in changes:
                    changed_path.write_text(contents)
            output = log.read_text(errors='replace')
            assert 'could not compile' not in output and 'running 0 tests' not in output, output[-6000:]
            if red:
                # Cargo propagates the WASM runner's exit 1; libtest uses 101.
                red_codes = (1, 101) if name == 'wasm-spool-owner-pin' else (101,)
                assert result.returncode in red_codes and 'FAILED' in output and assertion in output, output[-6000:]
            else:
                assert result.returncode == 0 and re.search(r'test result: ok\. [1-9][0-9]* passed;', output), output[-6000:]
            receipt = dict(guard=name, red=red, code=result.returncode, sha=sha, test=test,
                           command=command, log=str(log), seconds=round(time.monotonic()-start, 2))
            receipts.append(receipt)
            (args.output / 'results.json').write_text(json.dumps(receipts, indent=2)+'\n')
            print('PASS', label, receipt['seconds'], flush=True)
            assert all(changed_path.read_text() == contents for changed_path, contents, _, _ in changes)
    print('ALL', len(selected), 'FAIL-THEN-PASS PAIRS VERIFIED', flush=True)


if __name__ == '__main__':
    main()
