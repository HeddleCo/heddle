#!/usr/bin/env python3
"""Reproduce HYBRID signed corpora from the API pin; never edit signatures.

Usage: python3 scripts/regenerate-hybrid-alpha33.py [--api /path/to/api]
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


def import_roots(output):
    """Apply the owner's import ancestry rule to isolated tag generator inputs.

    Ordinary native vectors retain their child State. Imported captures preserve
    a parentless source State; every dependent signature is generated upstream.
    The API checkout itself is never modified.
    """
    path = output / 'tools/hybrid-native/src/main.rs'
    source = path.read_text()
    old = 'Some("child-state" | "descendant-state")'
    assert source.count(old) == 1
    source = source.replace(old, 'Some("child-state" | "descendant-state" | "import-root-state")')
    old = 'state.parents = vec![base];\n            state.intent = Some("Native hybrid child capture".into());'
    assert source.count(old) == 1
    source = source.replace(old, '''if args[1] == "import-root-state" {
                state.parents.clear();
                state.intent = Some("Native imported Git root capture".into());
            } else {
                state.parents = vec![base];
                state.intent = Some("Native hybrid child capture".into());
            }''')
    path.write_text(source)
    path = output / 'tools/generate-hybrid-fixture.mjs'
    source = path.read_text()
    old = "const content=commitment('content'"
    assert source.count(old) == 1
    source = source.replace(old, "const importRoot=JSON.parse(codecCall(['import-root-state'],encode(state)));\nconst importCapture={...capture,state:Array.from(Buffer.from(importRoot.state_hex,'hex'))};\n" + old)
    old = "canonicalCapture:nativeEncode('capture',capture)"
    assert source.count(old) == 1
    source = source.replace(old, "canonicalCapture:nativeEncode('capture',importCapture)")
    old = "result:capture,author:{kind:'local_key'}"
    # The other occurrence is ordinary local source authority, which stays strict.
    assert source.count(old) == 2
    source = source.replace(old, "result:importCapture,author:{kind:'local_key'}", 1)
    path.write_text(source)
    for generator in ['generate-hybrid-fixture', 'generate-import-sibling-jobs-alpha32',
                      'generate-native-witness-fixture']:
        run(['node', f'tools/{generator}.mjs'], output)


def writer_inputs(output):
    # The upstream maintenance generators take sealed, deterministic capabilities.
    # Keep their seed artifacts inside this isolated regeneration directory.
    manifest = 'tools/hybrid-native/Cargo.toml'
    seeds = [
        ('api-alpha35-cowriter-biscuit.binpb', '31313131-3131-3131-3131-313131313131', 65, 65),
        ('api-alpha35-fake-owner-biscuit.binpb', '21212121-2121-2121-2121-212121212121', 65, 65),
        ('api-alpha36-acceptor-biscuit.binpb', '21212121-2121-2121-2121-212121212121', 65, 67),
    ]
    for name, account, mint, publisher in seeds:
        run(['cargo', 'run', '--offline', '--locked', '--manifest-path', manifest,
             '--bin', 'generate-hybrid-native-biscuit', '--', str(output / name),
             account, str(mint), str(publisher)], output)
        for generator in ['generate-alpha35-writer-fixture', 'generate-alpha36-boundary-fixture']:
            path = output / 'tools' / (generator + '.mjs')
            path.write_text(path.read_text().replace('/tmp/' + name, str(output / name)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--api', type=Path, default=Path('/home/heddleco/HeddleCo/api'))
    parser.add_argument('--work-dir', type=Path, help='Reuse an isolated generator directory')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    pin = re.search(r'api = \{ package = "heddle-api".*rev = "([0-9a-f]{40})"', (root / 'Cargo.toml').read_text()).group(1)
    output = args.work_dir or Path(tempfile.mkdtemp(prefix='heddle-hybrid-'))
    output.mkdir(parents=True, exist_ok=True)
    output = output.resolve()
    archive = output / 'api.tar'
    with archive.open('wb') as target:
        run(['git', '-C', str(args.api), 'archive', pin], root, stdout=target)
    run(['tar', '-xf', str(archive), '-C', str(output)], root)
    archive.unlink()
    run(['npm', 'ci'], output)
    run(['npm', 'run', 'build'], output)
    for generator in ['generate-hybrid-fixture', 'generate-import-sibling-jobs-alpha32',
                      'generate-native-witness-fixture']:
        run(['node', f'tools/{generator}.mjs'], output)
    run(['node', 'tests/generate-hybrid-job-selector-fixture.mjs'], output)
    # Consumer additions deliberately preserve the earlier descriptor inventory.
    # Compare every generated record with the pinned frozen representation before
    # restoring only metadata/property order and installing any fixture copy.
    names = ['import-authority-host-witness-v1.json', 'native-host-witness-v1.json',
             'import-sibling-jobs-alpha32.json', 'hybrid-job-selector-v1.json',
             'hybrid-native-old-parentless-v1.json']
    for name in names:
        frozen = subprocess.check_output(['git', '-C', str(args.api), 'show', f'{pin}:tests/fixtures/{name}'])
        path = output / 'tests' / 'fixtures' / name
        generated = json.loads(path.read_bytes())
        expected = json.loads(frozen)
        excluded = {'messages', 'descriptors', 'enums'} if name == names[0] else set()
        assert {k: v for k, v in generated.items() if k not in excluded} == {k: v for k, v in expected.items() if k not in excluded}, name
        path.write_bytes(frozen)
    # Historical release manifests hash descriptor inventories from that release.
    # The exact current-pin signed-record comparison above is the regeneration gate.
    print('EXACT PIN SIGNED CORPORA VERIFIED', pin, flush=True)
    # First prove exact tag regeneration above; then generate Heddle's newly
    # specified imported-root positives without changing the signing formats.
    import_roots(output)
    writer_inputs(output)
    for generator in ['generate-alpha34-foreign-fixture', 'generate-alpha35-writer-fixture',
                      'generate-alpha36-boundary-fixture', 'generate-alpha37-attachment-fixture']:
        run(['node', f'tools/{generator}.mjs'], output)
    path = output / 'tests/fixtures' / names[0]
    generated = json.loads(path.read_bytes())
    frozen = json.loads(subprocess.check_output(['git', '-C', str(args.api), 'show', f'{pin}:tests/fixtures/{names[0]}']))
    for key in ['messages', 'descriptors', 'enums']:
        generated[key] = frozen[key]
    path.write_text(json.dumps(generated, indent=2) + '\n')
    targets = {
        names[0]: ['crates/capability-verifier/conformance/hybrid/' + names[0],
                   'crates/crypto/tests/fixtures/' + names[0],
                   'crates/repo/tests/fixtures/hybrid/' + names[0],
                   'crates/thread-api/tests/fixtures/hybrid-alpha33.json'],
        names[1]: [f'crates/{crate}/tests/fixtures/{names[1]}' for crate in ['capability-verifier', 'crypto', 'thread-api']],
        names[2]: ['crates/hosted-client/tests/fixtures/' + names[2]],
        names[3]: ['crates/hosted-client/tests/fixtures/' + names[3]],
        names[4]: ['crates/crypto/tests/fixtures/' + names[4]],
    }
    for name in ['foreign-dependencies-alpha34.json', 'writer-authority-alpha35.json',
                 'boundary-acceptor-alpha36.json']:
        targets[name] = ['crates/thread-api/tests/fixtures/' + name]
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
