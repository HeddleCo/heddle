# SPDX-License-Identifier: Apache-2.0
"""Create synthetic native Threads, project manifests and print ordinary Git commands."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
from .core import LocalCatalog, git_env


def setup(binary, root):
    root = Path(root).resolve()
    root.mkdir(parents=True, exist_ok=False)
    env = git_env()
    env.update(HEDDLE_HOME=str(root / 'home'), HEDDLE_PRINCIPAL_NAME='Synthetic Demo',
               HEDDLE_PRINCIPAL_EMAIL='demo@example.invalid')
    result = subprocess.run([str(binary), 'fixture', str(root / 'fixture')], env=env, check=True,
                            stdout=subprocess.PIPE, timeout=60)
    states = json.loads(result.stdout)
    native = root / 'fixture' / 'native'
    catalog = LocalCatalog(root / 'catalog.git')
    manifests = {}
    previous = None
    for name in ('base', 'merged', 'ordered'):
        mode = 'history' if name == 'ordered' else 'snapshot'
        with tempfile.TemporaryDirectory() as temp:
            oid = subprocess.check_output([str(binary), 'export', str(native), states[name],
                                           str(Path(temp) / 'view.git'), mode, 'main'], env=env, timeout=30).decode().strip()
        manifest = dict(schema=1, repository='synthetic-demo', source='native-demo', thread='main',
                        state=states[name], git_oid=oid, mode=mode, policy_epoch=1)
        pin = catalog.publish(manifest, previous)
        manifests[name] = {'pin': pin, 'manifest': manifest}
        previous = pin
    grants = [[m['manifest'][key] for key in ('repository', 'source', 'thread', 'state', 'policy_epoch')]
              for m in manifests.values()]
    policy = root / 'policy.json'
    policy.write_text(json.dumps({'readers': {'demo-reader': grants}}, indent=2) + '\n')
    (root / 'published-pins.json').write_text(json.dumps(catalog.published_pins()) + '\n')
    config = dict(catalog=str(catalog.path), binary=str(binary), sources={'native-demo': str(native)}, policy=str(policy))
    (root / 'config.json').write_text(json.dumps(config, indent=2) + '\n')
    evidence = dict(states=states, views=manifests, config=config)
    (root / 'evidence.json').write_text(json.dumps(evidence, indent=2) + '\n')
    return evidence


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--binary', type=Path, required=True)
    p.add_argument('--out', type=Path, required=True)
    args = p.parse_args()
    result = setup(args.binary.resolve(), args.out)
    print(json.dumps(result, indent=2))

if __name__ == '__main__':
    main()
