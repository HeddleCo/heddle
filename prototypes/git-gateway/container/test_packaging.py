# SPDX-License-Identifier: Apache-2.0
"""Static/image-command contracts. These tests do not execute a container engine."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
IMAGE_ID = 'sha256:' + 'a' * 64  # Explicitly public fake image identifier, not a credential.


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.config = self.root / 'config with spaces'
        self.config.mkdir()
        (self.config / 'host.json').write_text('{}\n')
        self.log = self.root / 'engine.jsonl'
        fake = self.root / 'docker'
        fake.write_text(f'''#!{sys.executable}
import json, os, sys
with open(os.environ['PACKAGING_ENGINE_LOG'], 'a') as out:
    out.write(json.dumps(sys.argv[1:]) + '\\n')
if sys.argv[1:3] == ['image', 'inspect']:
    if os.environ.get('PACKAGING_MISSING_IMAGE') == '1':
        sys.exit(1)
    print({IMAGE_ID!r})
elif sys.argv[1] != 'run':
    sys.exit('Unexpected container-engine operation')
''')
        fake.chmod(0o755)
        self.env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.defpath,
                        PACKAGING_ENGINE_LOG=str(self.log))

    def script(self, name, *args):
        result = subprocess.run(['/bin/sh', str(HERE / name), *map(str, args)],
                                env=self.env, capture_output=True, text=True, timeout=5)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, calls

    def assert_hardened(self, call):
        for required in ('--pull=never', '--rm', '--user=65532:65532', '--read-only',
                         '--cap-drop=ALL', '--security-opt=no-new-privileges:true',
                         '--pids-limit=64', '--memory=1g', '--memory-swap=1g', '--cpus=1',
                         '--ulimit=nofile=128:128', '--ulimit=nproc=64:64',
                         '--ulimit=fsize=100663296:100663296', '--ulimit=core=0:0',
                         '--shm-size=16m',
                         '--tmpfs=/tmp:rw,noexec,nosuid,nodev,size=512m,mode=0700,uid=65532,gid=65532'):
            self.assertIn(required, call)
        for argument in call:
            self.assertFalse(argument.startswith(('--publish', '--privileged', '--network=host')))
        self.assertNotIn('-p', call)
        self.assertNotIn('-P', call)
        self.assertIn(IMAGE_ID, call)

    def test_run_uses_fixed_limits_and_readonly_configuration(self):
        result, calls = self.script('run.sh', self.config, 'heddle-gateway:local')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0], ['image', 'inspect', '--format', '{{.Id}}', 'heddle-gateway:local'])
        self.assertEqual(len(calls), 2)
        self.assert_hardened(calls[1])
        self.assertIn('--network=bridge', calls[1])
        self.assertIn(f'--mount=type=bind,src={self.config},dst=/config,readonly,bind-recursive=disabled', calls[1])
        self.assertEqual(calls[1][-1], IMAGE_ID)

    def test_smoke_is_offline_and_has_no_config_or_credentials(self):
        result, calls = self.script('smoke.sh', 'heddle-gateway:local')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assert_hardened(calls[1])
        self.assertIn('--network=none', calls[1])
        self.assertIn('--entrypoint=python3', calls[1])
        self.assertEqual(calls[1][-2:], [IMAGE_ID, '-'])
        self.assertFalse(any(arg.startswith(('--mount', '--env', '-e=')) for arg in calls[1]))

    def test_missing_image_never_builds_pulls_or_runs(self):
        self.env['PACKAGING_MISSING_IMAGE'] = '1'
        result, calls = self.script('smoke.sh', 'absent:local')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][:2], ['image', 'inspect'])

    def test_missing_host_config_stops_before_engine(self):
        (self.config / 'host.json').unlink()
        result, calls = self.script('run.sh', self.config, 'local')
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])

    def test_mount_separator_in_path_is_rejected(self):
        config = self.root / 'bad,readonly=false'
        config.mkdir()
        (config / 'host.json').write_text('{}\n')
        result, calls = self.script('run.sh', config, 'local')
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])

    def test_image_option_injection_is_rejected(self):
        result, calls = self.script('smoke.sh', '--privileged')
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])

    def test_extra_engine_flags_cannot_be_supplied(self):
        result, calls = self.script('run.sh', self.config, 'local', '--privileged')
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])

    def test_shell_syntax(self):
        for filename in ('run.sh', 'smoke.sh', 'runtime.sh'):
            result = subprocess.run(['/bin/sh', '-n', str(HERE / filename)], capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_locked_multistage_build_and_explicit_runtime_allowlist(self):
        dockerfile = (HERE / 'Dockerfile').read_text()
        self.assertIn('FROM rust:1.98.0-trixie AS build', dockerfile)
        self.assertIn('cargo build --locked --release -p heddle-git-projection --example gateway_native', dockerfile)
        self.assertIn('FROM debian:trixie-slim AS runtime', dockerfile)
        self.assertIn('test -x "$(git --exec-path)/git-http-backend"', dockerfile)
        self.assertIn('USER 65532:65532', dockerfile)
        self.assertIn('ENTRYPOINT ["python3", "-m", "gateway.host", "/config/host.json"]', dockerfile)
        logical_lines = dockerfile.replace('\\\n', '').splitlines()
        runtime = False
        sources = []
        for line in logical_lines:
            if line.startswith('FROM ') and 'AS runtime' in line:
                runtime = True
            if line.startswith('COPY '):
                fields = shlex.split(line)
                paths = [part for part in fields[1:] if not part.startswith('--')]
                if runtime and '--from=build' not in fields:
                    sources.extend(paths[:-1])
                    for path in paths[:-1]:
                        self.assertTrue((ROOT / path).is_file(), path)
        expected = {'LICENSE'} | {
            'prototypes/git-gateway/gateway/' + name + '.py'
            for name in ('__init__', 'artifacts', 'auth', 'bundle_transport', 'child', 'core', 'host', 'http', 'native_bundle')
        }
        self.assertEqual(set(sources), expected)
        self.assertFalse(any(line.startswith(('ADD ', 'VOLUME ', 'EXPOSE ', 'ARG ')) for line in logical_lines))

    def test_dockerfile_specific_context_denies_local_state(self):
        rules = [line for line in (HERE / 'Dockerfile.dockerignore').read_text().splitlines()
                 if line and not line.startswith('#')]
        self.assertEqual(rules[0], '**')
        self.assertIn('**/.git', rules)
        self.assertIn('**/.heddle', rules)
        self.assertIn('**/.env*', rules)
        self.assertNotIn('!prototypes/git-gateway/**', rules)
        self.assertNotIn('!prototypes/git-gateway/gateway/demo.py', rules)
        self.assertFalse(any('!' + name in rules for name in ('target/', '.demo/', '.git/')))


if __name__ == '__main__':
    unittest.main()
