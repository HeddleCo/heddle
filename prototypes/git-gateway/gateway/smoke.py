# SPDX-License-Identifier: Apache-2.0
"""Repeatable LOCAL proof: Worker harness -> native bundle -> real Git clone/fetch.

Cloudflare-shaped callbacks and public test vectors are intentional. This module
never deploys, uploads, reads account credentials or demonstrates live bindings.
"""
import argparse
import hashlib
import json
from pathlib import Path
import select
import shutil
import subprocess
import tempfile
import tarfile
import io
import tomllib
import threading
import time

from .auth import BearerAuthorization, ServiceAuthorization
from .core import GIT, LocalCatalog, Refused, canonical, git, git_env, thread_grants
from .demo import setup
from .http import server
from .native_bundle import MAX_BUNDLE, NativeBundleSource, pack_fixture

READER = 'PUBLIC_TEST_VECTOR_READER_NOT_SECRET_0000000'
SERVICE = 'PUBLIC_TEST_VECTOR_SERVICE_NOT_SECRET_000000'
SCOPE = ('repository', 'source', 'thread', 'state', 'policy_epoch')


def command(args, label, *, check=True):
    result = subprocess.run(args, env=git_env(), capture_output=True, timeout=90)
    if check and result.returncode:
        raise Refused(label + ' failed')
    return result


def inputs(binary, directory, dataset=None):
    if dataset is None:
        evidence = setup(binary, directory / 'generated')
        bundle = pack_fixture(evidence['config']['sources']['native-demo'])
        views = {name: evidence['views'][name] for name in ('base', 'merged')}
        catalog = LocalCatalog(evidence['config']['catalog'])
        # Remove the generated native repository, working trees and identity.
        # Only the transport bytes and metadata catalog can serve the next request.
        shutil.rmtree(directory / 'generated' / 'fixture')
        shutil.rmtree(directory / 'generated' / 'home')
        return bundle, views, catalog, {'native-demo': ['main']}, 'fresh synthetic native captures'
    dataset = Path(dataset)
    data = json.loads((dataset / 'DATA.json').read_bytes())
    if data.get('synthetic_only') is not True or data.get('authority_included') is not False:
        raise Refused('only an explicitly synthetic, authority-free dataset is accepted')
    bundle_path = dataset / 'native.bundle'
    if bundle_path.stat().st_size > MAX_BUNDLE:
        raise Refused('native bundle limit')
    bundle = bundle_path.read_bytes()
    if (hashlib.sha256(bundle).hexdigest() != data['native_bundle_sha256'] or
            len(bundle) != data['native_bundle_bytes']):
        raise Refused('dataset native bundle mismatch')
    with tarfile.open(fileobj=io.BytesIO(bundle), mode='r:') as archive:
        config = archive.extractfile('.heddle/config.toml')
        if config is None:
            raise Refused('native format declaration missing')
        raw = config.read(65537)
        if len(raw) > 65536:
            raise Refused('native configuration limit')
        found = tomllib.loads(raw.decode())['repository']['version']
    supported = int(command([str(binary), 'format'], 'native format probe').stdout)
    if found != supported:
        raise Refused(f'dataset format v{found} cannot run on native v{supported}; rebuild the synthetic demo, never edit its version field')
    catalog_bundle = dataset / 'catalog.bundle'
    if catalog_bundle.stat().st_size > MAX_BUNDLE or hashlib.sha256(catalog_bundle.read_bytes()).hexdigest() != data['catalog_bundle_sha256']:
        raise Refused('dataset catalog bundle mismatch')
    views = data['views']
    if set(views) != {'base', 'integrated'}:
        raise Refused('dataset requires exact base and integrated views')
    path = directory / 'catalog.git'
    command([GIT, 'clone', '--bare', str(catalog_bundle.resolve()), str(path)], 'catalog restore')
    catalog = LocalCatalog(path)
    for view in views.values():
        if catalog.resolve(view['pin']) != view['manifest']:
            raise Refused('dataset manifest mismatch')
    names = list(thread_grants({data['source']: data['authorized_threads']})[data['source']])
    return bundle, views, catalog, {data['source']: names}, 'recovered synthetic agent dataset'


def run(binary, output, dataset=None):
    binary, output = Path(binary).resolve(), Path(output).resolve()
    if not binary.is_file():
        raise Refused('build gateway_native first')
    output.mkdir(parents=True, exist_ok=False)
    report = {'schema': 1, 'status': 'running', 'evidence_kind': 'local-composition-only',
        'live_cloudflare_verified': False, 'external_service_requests': 0,
        'checks': [], 'versions': {'git': command([GIT, '--version'], 'git version').stdout.decode().strip(),
            'node': command([shutil.which('node'), '--version'], 'node version').stdout.decode().strip()}}
    report_path = output / 'report.json'
    app = thread = worker = None
    try:
        with tempfile.TemporaryDirectory(prefix='heddle-smoke-') as scratch:
            root = Path(scratch)
            bundle, views, catalog, grants, provenance = inputs(binary, root, dataset)
            source_name = next(iter(grants))
            digest = hashlib.sha256(bundle).hexdigest()
            report.update(source=provenance, native_bundle_sha256=digest,
                native_bundle_bytes=len(bundle), views=views)
            policy, services = root / 'readers.json', root / 'services.json'
            expiry = int(time.time()) + 900
            reader_policy = {'readers': [{'sha256': hashlib.sha256(READER.encode()).hexdigest(),
                'expires_at': expiry, 'views': [[v['manifest'][k] for k in SCOPE] for v in views.values()]}]}
            policy.write_text(json.dumps(reader_policy))
            services.write_text(json.dumps({'services': [{'sha256': hashlib.sha256(SERVICE.encode()).hexdigest(),
                'expires_at': expiry, 'pins': [v['pin'] for v in views.values()]}]}))
            reads = []
            def read_bundle(name):
                if name != source_name:
                    raise Refused('unconfigured source')
                reads.append(name)
                return bundle
            source = NativeBundleSource(binary, {source_name: digest}, read_bundle, grants)
            app = server(catalog, source, BearerAuthorization(policy), bearer_mode=True,
                service_authorization=ServiceAuthorization(services))
            thread = threading.Thread(target=app.serve_forever, daemon=True)
            thread.start()
            fixture = root / 'worker.json'
            fixture.write_text(json.dumps({'port': app.server_port,
                'manifests': {v['pin']: canonical(v['manifest']).decode() for v in views.values()}}))
            harness = Path(__file__).resolve().parent.parent / 'worker' / 'local-host-smoke.mjs'
            worker = subprocess.Popen([shutil.which('node'), str(harness), str(fixture)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            ready, _, _ = select.select([worker.stdout], [], [], 10)
            if not ready:
                raise Refused('local Worker startup deadline')
            line = worker.stdout.readline().strip()
            prefix = 'LOCAL_FIXTURE_WORKER_PORT='
            if not line.startswith(prefix) or not line[len(prefix):].isdigit():
                raise Refused('local Worker failed to start')
            origin = 'http://127.0.0.1:' + line[len(prefix):]
            def url(name):
                return origin + '/views/' + views[name]['pin'] + '.git'
            client = [GIT, '-c', 'http.extraHeader=Authorization: Bearer ' + READER]
            newer = 'integrated' if 'integrated' in views else 'merged'
            clone = output / 'clone'
            command(client + ['clone', url('base'), str(clone)], 'base clone')
            if command([GIT, '-C', str(clone), 'rev-parse', 'HEAD'], 'base HEAD').stdout.decode().strip() != views['base']['manifest']['git_oid']:
                raise Refused('base Git OID mismatch')
            report['checks'].append('base clone exact OID')
            command(client + ['-C', str(clone), 'fetch', url(newer), 'refs/heads/main:refs/remotes/view/main'], 'new view fetch')
            command([GIT, '-C', str(clone), 'checkout', '--detach', 'refs/remotes/view/main'], 'new view checkout')
            fresh = output / 'fresh-clone'
            command(client + ['clone', url(newer), str(fresh)], 'cold fresh clone')
            for path in (clone, fresh):
                actual = command([GIT, '-C', str(path), 'rev-parse', 'HEAD'], 'projected HEAD').stdout.decode().strip()
                if actual != views[newer]['manifest']['git_oid']:
                    raise Refused('projected Git OID mismatch')
                command([GIT, '-C', str(path), 'fsck', '--strict'], 'strict fsck')
            report['checks'] += ['fetch exact OID', 'fresh clone exact OID', 'strict fsck both clones',
                'native bytes only; fresh extraction and Git projection each request']
            before = len(reads)
            denied = command([GIT, 'ls-remote', url(newer)], 'unauthenticated discovery', check=False)
            if denied.returncode == 0 or len(reads) != before:
                raise Refused('unauthenticated request reached native source')
            report['checks'].append('unauthenticated request denied before native bytes')
            policy.write_text('{"readers": []}')
            if command(client + ['ls-remote', url(newer)], 'reader revocation', check=False).returncode == 0 or len(reads) != before:
                raise Refused('reader revocation failed')
            report['checks'].append('reader revocation before native bytes')
            policy.write_text(json.dumps(reader_policy))
            services.write_text('{"services": []}')
            if command(client + ['ls-remote', url(newer)], 'service revocation', check=False).returncode == 0 or len(reads) != before:
                raise Refused('service revocation failed')
            report['checks'].append('independent service revocation before native bytes')
            report.update(status='passed', native_bundle_reads=len(reads),
                caveat='Artifacts and R2 are explicit local fixtures; no TLS, VM, live Cloudflare, or competition eligibility proof.')
    except Exception:
        report['status'] = 'failed'
        raise
    finally:
        if worker is not None:
            worker.terminate()
            try:
                worker.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                worker.kill(); worker.communicate()
        if app is not None:
            app.shutdown(); thread.join(); app.server_close()
        report_path.write_text(json.dumps(report, indent=2) + '\n')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True, help='new directory for clones and report')
    parser.add_argument('--dataset', type=Path, help='optional verified synthetic DATA.json/bundle directory')
    args = parser.parse_args()
    print(json.dumps(run(args.binary, args.out, args.dataset), indent=2))


if __name__ == '__main__':
    main()
