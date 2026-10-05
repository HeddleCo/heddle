#!/usr/bin/env python3
"""Runtime guard-removal proofs on an isolated copy of committed Heddle sources.

Every mutation must fail the named assertion, then pass after byte-exact source
restoration. Compilation errors and zero selected tests are never evidence.
Run after committing: python3 scripts/prove-native-witness-guards.py [guard ...]
"""
import argparse
import json
import os
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
# name, file, exact old bytes, replacement, crate, filter, expected assertion
MUTATIONS = [
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
    ('binding-ambiguity', 'crates/thread-api/src/hybrid/authority.rs', '''                        if resolved.is_some() {
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
]

# The import composition helper also enforces the durable witness checkpoint.
# Remove both checks to prove the family; removing one leaves the other active.
EXTRA_MUTATIONS = {
    'part2-durable-witness': [
        ('crates/repo/src/thread_replication/delegated_import.rs',
         '        snapshot.as_ref(),', '        None,'),
    ],
}


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
    target = os.environ.get('NATIVE_PROOF_TARGET', str(args.output / 'target'))
    receipts = []
    selected = [m for m in MUTATIONS if not args.guards or m[0] in args.guards]
    assert selected and set(args.guards) <= {m[0] for m in MUTATIONS}
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
            command = ['cargo', 'test', '--locked', '-p', crate, '--lib']
            if crate == HOSTED:
                command += ['--features', 'client']
            command += [test, '--', '--nocapture', '--test-threads', '1']
            log = args.output / (label + '.log')
            env = {**os.environ, 'CARGO_TARGET_DIR': target,
                   'HEDDLE_HOME': tempfile.mkdtemp(prefix='home-', dir=args.output)}
            start = time.monotonic()
            print('RUN', label, flush=True)
            try:
                if red:
                    for changed_path, contents, before, after in changes:
                        changed_path.write_text(contents.replace(before, after))
                with log.open('w') as output:
                    result = subprocess.run(command, cwd=source, env=env, stdout=output, stderr=subprocess.STDOUT)
            finally:
                for changed_path, contents, _, _ in changes:
                    changed_path.write_text(contents)
            output = log.read_text(errors='replace')
            assert 'could not compile' not in output and 'running 0 tests' not in output, output[-6000:]
            if red:
                assert result.returncode == 101 and 'FAILED' in output and assertion in output, output[-6000:]
            else:
                assert result.returncode == 0, output[-6000:]
            receipt = dict(guard=name, red=red, code=result.returncode, sha=sha, test=test,
                           command=command, log=str(log), seconds=round(time.monotonic()-start, 2))
            receipts.append(receipt)
            (args.output / 'results.json').write_text(json.dumps(receipts, indent=2)+'\n')
            print('PASS', label, receipt['seconds'], flush=True)
            assert all(changed_path.read_text() == contents for changed_path, contents, _, _ in changes)
    print('ALL', len(selected), 'FAIL-THEN-PASS PAIRS VERIFIED', flush=True)


if __name__ == '__main__':
    main()
