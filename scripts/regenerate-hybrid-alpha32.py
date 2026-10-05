#!/usr/bin/env python3
"""Reproduce alpha.32 signed corpora from the API pin; never edit signatures.

Usage: python3 scripts/regenerate-hybrid-alpha32.py [--api /path/to/api]
Requires npm, buf and the API native maintenance tool's Rust dependencies.
"""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def run(command, cwd, **kwargs):
    print('RUN', ' '.join(map(str, command)), flush=True)
    return subprocess.run(command, cwd=cwd, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--api', type=Path, default=Path('/home/heddleco/HeddleCo/api'))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    pin = re.search(r'api = \{ package = "heddle-api".*rev = "([0-9a-f]{40})"', (root / 'Cargo.toml').read_text()).group(1)
    output = Path(tempfile.mkdtemp(prefix='heddle-alpha32-'))
    archive = output / 'api.tar'
    with archive.open('wb') as target:
        run(['git', '-C', str(args.api), 'archive', pin], root, stdout=target)
    run(['tar', '-xf', str(archive), '-C', str(output)], root)
    archive.unlink()
    run(['npm', 'ci'], output)
    run(['npm', 'run', 'build'], output)
    for generator in ['generate-hybrid-fixture', 'generate-alpha31-review-fixture',
                      'generate-import-job-control-alpha31', 'generate-alpha32-fixture',
                      'generate-import-sibling-jobs-alpha32', 'generate-import-consumer-alpha32']:
        run(['node', f'tools/{generator}.mjs'], output)
    # Consumer additions deliberately preserve the earlier descriptor inventory.
    # Compare every generated record with the pinned frozen representation before
    # restoring only metadata/property order and installing any fixture copy.
    names = ['import-authority-host-witness-v1.json', 'native-host-witness-v1.json',
             'import-job-control-alpha31.json', 'import-sibling-jobs-alpha32.json',
             'import-consumer-alpha32.json']
    for name in names:
        frozen = subprocess.check_output(['git', '-C', str(args.api), 'show', f'{pin}:tests/fixtures/{name}'])
        path = output / 'tests' / 'fixtures' / name
        generated = json.loads(path.read_bytes())
        expected = json.loads(frozen)
        excluded = {'messages', 'descriptors', 'enums'} if name == names[0] else set()
        assert {k: v for k, v in generated.items() if k not in excluded} == {k: v for k, v in expected.items() if k not in excluded}, name
        path.write_bytes(frozen)
    run(['node', 'tools/verify-alpha32-vector-continuity.mjs'], output)
    targets = {
        names[0]: ['crates/capability-verifier/conformance/hybrid/' + names[0],
                   'crates/crypto/tests/fixtures/' + names[0],
                   'crates/repo/tests/fixtures/hybrid/' + names[0],
                   'crates/thread-api/tests/fixtures/hybrid-alpha32.json'],
        names[1]: [f'crates/{crate}/tests/fixtures/{names[1]}' for crate in ['capability-verifier', 'crypto', 'thread-api']],
        **{name: ['crates/hosted-client/tests/fixtures/' + name] for name in names[2:]},
    }
    for name, destinations in targets.items():
        for destination in destinations:
            shutil.copyfile(output / 'tests' / 'fixtures' / name, root / destination)
    log = output / 'claimed-owner.log'
    with log.open('w') as target:
        run(['cargo', 'test', '--locked', '-p', 'heddleco-capability-verifier',
             'print_claimed_owner_fixture_json', '--', '--ignored', '--nocapture'], root,
            stdout=target, stderr=subprocess.STDOUT)
    line = next(line for line in log.read_text().splitlines() if line.startswith('CLAIMED_OWNER_FIXTURE='))
    claimed = json.loads(line.split('=', 1)[1])
    (root / 'crates/capability-verifier/conformance/hybrid/claimed-owner-expiry-v1.json').write_text(json.dumps(claimed, indent=2, sort_keys=True) + '\n')
    print('REGENERATED API', pin, 'IN', output, flush=True)


if __name__ == '__main__':
    main()
