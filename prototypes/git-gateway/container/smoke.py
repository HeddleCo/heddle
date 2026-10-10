# SPDX-License-Identifier: Apache-2.0
"""Runtime packaging assertions only; no fixture identities or hosted calls."""
import importlib
import os
from pathlib import Path
import resource
import subprocess
import tempfile

assert os.getuid() == os.getgid() == 65532
assert Path.cwd() == Path('/app')
for name in ('artifacts', 'auth', 'bundle_transport', 'core', 'host', 'http', 'native_bundle'):
    importlib.import_module('gateway.' + name)
for path in (Path('/app'), Path('/app/gateway'), Path('/usr/local/bin/gateway_native')):
    assert not os.access(path, os.W_OK), f'{path} must not be writable'
# Mount flags, rather than Unix mode bits alone, must make the root read-only.
mounts = [line.split() for line in Path('/proc/mounts').read_text().splitlines()]
assert any(row[1] == '/' and 'ro' in row[3].split(',') for row in mounts)
assert any(row[1] == '/tmp' and row[2] == 'tmpfs' and
           {'noexec', 'nosuid', 'nodev'} <= set(row[3].split(',')) for row in mounts)
status = dict(line.split(':', 1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
assert status['NoNewPrivs'].strip() == '1'
assert int(status['CapEff'].strip(), 16) == 0
assert resource.getrlimit(resource.RLIMIT_NOFILE) == (128, 128)
assert resource.getrlimit(resource.RLIMIT_CORE) == (0, 0)
# This smoke deliberately requires cgroup v2 so ignored engine limits fail closed.
cgroup = Path('/sys/fs/cgroup')
assert (cgroup / 'cgroup.controllers').is_file(), 'Smoke requires Linux cgroup v2'
assert (cgroup / 'memory.max').read_text().strip() == str(1024 * 1024 * 1024)
assert (cgroup / 'memory.swap.max').read_text().strip() == '0'
assert (cgroup / 'pids.max').read_text().strip() == '64'
quota, period = (cgroup / 'cpu.max').read_text().split()
assert quota != 'max' and int(quota) == int(period)
with tempfile.TemporaryDirectory() as directory:
    assert Path(directory).parent == Path('/tmp')
    (Path(directory) / 'probe').write_text('scratch only\n')
backend = Path(subprocess.check_output(['git', '--exec-path'], text=True).strip()) / 'git-http-backend'
assert backend.is_file() and os.access(backend, os.X_OK)
# No arguments take the usage/error path, without opening or creating a repository.
probe = subprocess.run(['/usr/local/bin/gateway_native'], capture_output=True, timeout=10)
assert probe.returncode != 0 and b'usage: gateway_native' in probe.stderr, probe.stderr
# These files are absent by design; smoke does not mount or generate any policy.
assert not Path('/config/host.json').exists()
assert not Path('/app/gateway/demo.py').exists()
print('Container packaging smoke passed; hosted operation and cgroup stress limits remain unverified')
