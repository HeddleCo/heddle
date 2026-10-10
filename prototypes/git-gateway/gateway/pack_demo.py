# SPDX-License-Identifier: Apache-2.0
"""Build a LOCAL metadata catalog and identity-free bundle for a synthetic demo.

No upload/publication to any remote. The caller must explicitly identify the
input as synthetic and supply the signed governing and dependency Thread scope.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import shutil
import sqlite3
import tempfile
from .core import LocalCatalog, Refused, canonical, git, git_env, thread_grants
from .native_bundle import pack_fixture, unpack_fixture


def pack(binary, native, base, integrated, output, *, threads, thread='demo/priority-sort'):
    binary, native, output = Path(binary).resolve(), Path(native).resolve(), Path(output).resolve()
    grants = list(thread_grants({'native-app': threads})['native-app'])
    if 'main' not in grants or thread not in grants:
        raise Refused('base and integrated governing Threads must be explicitly granted')
    output.mkdir(parents=True, exist_ok=False)
    # Only this explicitly synthetic packer removes operational local metadata.
    # Native object/attachment bytes and signed SQLite originals are unchanged.
    if any((native / '.heddle' / marker).exists() for marker in ('objectstore', 'lazy-hydrator.toml')):
        raise Refused('external native storage is unsupported for this synthetic packer')
    initial = pack_fixture(native)
    with tempfile.TemporaryDirectory(prefix='heddle-demo-pack-') as temporary:
        temporary = Path(temporary)
        unpack_fixture(initial, temporary / 'native')
        metadata = temporary / 'native' / '.heddle'
        removed = []
        for name in ('thread_workspaces', 'thread_records', 'threads', 'oplog', 'locks',
                'RECONCILE_WATERMARK_LOCAL', 'RECONCILE_WATERMARK_SHARED',
                'SNAPSHOT_WITNESS_LOCAL', 'SNAPSHOT_WITNESS_SHARED',
                'metadata.initialize.lock', 'metadata.sqlite3.changed'):
            path = metadata / name
            if path.is_dir():
                shutil.rmtree(path); removed.append(name)
            elif path.exists():
                path.unlink(); removed.append(name)
        # Compact unused SQLite pages without changing any logical native row.
        database = metadata / 'metadata.sqlite3'
        connection = sqlite3.connect(database)
        try:
            if connection.execute('PRAGMA integrity_check').fetchone() != ('ok',):
                raise Refused('native metadata integrity failure')
            connection.execute('VACUUM')
            checkpoint = connection.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone()
            if checkpoint[0] != 0:
                raise Refused('native metadata checkpoint unavailable')
        finally:
            connection.close()
        if any((metadata / name).exists() for name in ('metadata.sqlite3-wal', 'metadata.sqlite3-shm')):
            raise Refused('native metadata must be closed before bundling')
        bundle = pack_fixture(temporary / 'native')
        digest = hashlib.sha256(bundle).hexdigest()
        catalog = LocalCatalog(temporary / 'catalog.git')
        views, previous = {}, None
        for label, state, owner in [('base', base, 'main'), ('integrated', integrated, thread)]:
            env = git_env()
            env.update(HEDDLE_HOME=str(temporary / 'home'), HEDDLE_PRINCIPAL_NAME='Synthetic Demo', HEDDLE_PRINCIPAL_EMAIL='demo@example.invalid')
            result = subprocess.run([str(binary), 'export', str(temporary / 'native'), state,
                str(temporary / (label + '.git')), 'snapshot', owner, json.dumps(grants)],
                env=env, capture_output=True, text=True, timeout=30)
            if result.returncode:
                raise Refused('synthetic native projection refused; check format, signature, Thread and state')
            manifest = dict(schema=1, repository='actual-agent-demo-v6', source='native-app',
                thread=owner, state=state, git_oid=result.stdout.strip(), mode='snapshot', policy_epoch=1)
            previous = catalog.publish(manifest, previous)
            views[label] = {'pin': previous, 'manifest': manifest}
        git(catalog.path, 'bundle', 'create', str(output / 'catalog.bundle'), 'refs/heads/catalog')
    (output / 'native.bundle').write_bytes(bundle)
    manifests = output / 'manifests'; manifests.mkdir()
    for label, view in views.items():
        (manifests / (label + '.json')).write_bytes(canonical(view['manifest']))
    data = dict(schema=1, purpose='Explicitly synthetic actual-agent v6 rehearsal; local-only',
        source='native-app', native_bundle_sha256=digest, native_bundle_bytes=len(bundle),
        r2_object_key=f'native/{digest}.bundle',
        catalog_bundle_sha256=hashlib.sha256((output / 'catalog.bundle').read_bytes()).hexdigest(),
        catalog_ref='refs/heads/catalog', views=views, authorized_threads=grants,
        authority_included=False, synthetic_only=True,
        semantics='Integrated governing Thread is '+thread+'; projected Git main is a derived snapshot, not native main admission.',
        sanitization='Excluded signing identity and projection caches, local workspace/record paths, oplog, top-level operational locks and recovery markers; SQLite compacted without logical native-row changes. Native object and signed-original bytes retained. Synthetic-only, not a general private-repository sanitizer.', removed_operational_metadata=removed)
    (output / 'DATA.json').write_text(json.dumps(data, indent=2) + '\n')
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--native', type=Path, required=True)
    parser.add_argument('--base', required=True)
    parser.add_argument('--integrated', required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--thread', default='demo/priority-sort')
    parser.add_argument('--authorized-thread', action='append', required=True)
    parser.add_argument('--synthetic', action='store_true', required=True,
        help='confirm this local input contains synthetic demo content only')
    args = parser.parse_args()
    print(json.dumps(pack(args.binary, args.native, args.base, args.integrated, args.out,
        threads=args.authorized_thread, thread=args.thread), indent=2))


if __name__ == '__main__':
    main()
