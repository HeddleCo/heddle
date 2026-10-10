# SPDX-License-Identifier: Apache-2.0
"""Small local catalog/source boundaries; no hosted storage or identity impersonation."""
import json
import os
from pathlib import Path
import re
import signal
import sys
import stat
import shutil
import subprocess
import tempfile

GIT = shutil.which('git')
OID = re.compile(r'[0-9a-f]{40}')
STATE = re.compile(r'hs-[0-9a-z]{52}')
NAME = re.compile(r'[a-z][a-z0-9-]{0,63}')
THREAD = re.compile(r'[a-z][a-z0-9-]{0,63}(?:/[a-z][a-z0-9-]{0,63}){0,7}')
MAX_MANIFEST = 16 * 1024
MAX_DISK = 96 * 1024 * 1024

def bounded_process(command, **kwargs):
    # Limits are installed in a fresh interpreter, avoiding preexec_fn after fork
    # in a multithreaded embedding. exec preserves the PID; timeouts kill its group.
    command = [sys.executable, str(Path(__file__).with_name('child.py')), *command]
    timeout = kwargs.pop('timeout', 30)
    data = kwargs.pop('input', None)
    with subprocess.Popen(command, stdin=subprocess.PIPE, start_new_session=True, **kwargs) as process:
        try:
            stdout, stderr = process.communicate(data, timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
            raise
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


class Refused(Exception):
    pass


def git_env():
    # Never inherit alternates, config injection, credentials, hooks or Git directories.
    return {'PATH': os.defpath, 'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_TERMINAL_PROMPT': '0',
            'GIT_AUTHOR_NAME': 'Synthetic Catalog', 'GIT_AUTHOR_EMAIL': 'catalog@example.invalid',
            'GIT_COMMITTER_NAME': 'Synthetic Catalog', 'GIT_COMMITTER_EMAIL': 'catalog@example.invalid',
            'GIT_AUTHOR_DATE': '1700000000 +0000', 'GIT_COMMITTER_DATE': '1700000000 +0000',
            'LC_ALL': 'C'}


def git(repo, *args, data=None, check=True):
    p = subprocess.run([GIT, '--git-dir', str(repo), *args], input=data,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=git_env(), timeout=20)
    if check and p.returncode:
        raise Refused('Git operation failed: ' + p.stderr.decode(errors='replace')[:200])
    return p.stdout if check else p


def canonical(manifest):
    data = (json.dumps(manifest, sort_keys=True, separators=(',', ':')) + '\n').encode()
    validate(data)
    return data


def validate(data):
    if len(data) > MAX_MANIFEST:
        raise Refused('manifest limit')
    try:
        m = json.loads(data)
        if not isinstance(m, dict):
            raise ValueError()
        if set(m) != {'schema', 'repository', 'source', 'state', 'git_oid', 'mode', 'policy_epoch', 'thread'}:
            raise ValueError()
        if type(m['schema']) is not int or m['schema'] != 1 or m['mode'] not in ('snapshot', 'history'):
            raise ValueError()
        for key in ('repository', 'source'):
            if not isinstance(m[key], str) or not NAME.fullmatch(m[key]):
                raise ValueError()
        if not isinstance(m['thread'], str) or len(m['thread']) > 255 or not THREAD.fullmatch(m['thread']):
            raise ValueError()
        if not OID.fullmatch(m['git_oid']) or not STATE.fullmatch(m['state']):
            raise ValueError()
        if type(m['policy_epoch']) is not int or not 1 <= m['policy_epoch'] <= 9007199254740991:
            raise ValueError()
        # One canonical encoding across Python and Worker: rejects duplicate keys,
        # ambiguous JSON and whitespace variants before any manifest is trusted.
        if data != (json.dumps(m, sort_keys=True, separators=(',', ':')) + '\n').encode():
            raise ValueError()
    except (ValueError, TypeError, KeyError):
        raise Refused('invalid manifest') from None
    return m


class LocalCatalog:
    """A real, metadata-only Git repo standing in for an Artifacts catalog.

    The published ref is atomically CAS-updated; an immutable commit is the pin.
    Unpublished CAS losers are never accepted by resolve(). Replaying identical
    bytes + expected parent recovers a lost acknowledgment, even after advancement.
    """
    def __init__(self, path):
        self.path = Path(path)
        if not self.path.exists():
            git(self.path, 'init', '--bare', str(self.path))

    def head(self):
        p = git(self.path, 'rev-parse', '--verify', 'refs/heads/catalog', check=False)
        return p.stdout.decode().strip() if p.returncode == 0 else None

    def publish(self, manifest, expected=None):
        if expected is not None and not OID.fullmatch(expected):
            raise Refused('invalid parent')
        data = canonical(manifest)
        blob = git(self.path, 'hash-object', '-w', '--stdin', data=data).strip()
        tree = git(self.path, 'mktree', data=b'100644 blob ' + blob + b'\tmanifest.json\n').strip().decode()
        args = ['commit-tree', tree]
        if expected:
            args += ['-p', expected]
        oid = git(self.path, *args, data=b'Publish native view\n').decode().strip()
        current = self.head()
        if current and git(self.path, 'merge-base', '--is-ancestor', oid, current, check=False).returncode == 0:
            return oid  # lost acknowledgment; this exact publication already committed
        p = git(self.path, 'update-ref', 'refs/heads/catalog', oid, expected or '0' * 40, check=False)
        if p.returncode:
            raise Refused('concurrent publication: expected head changed')
        git(self.path, 'symbolic-ref', 'HEAD', 'refs/heads/catalog')
        return oid

    def published_pins(self):
        """Trusted publication snapshot for the read-only Artifacts adapters.

        Supply this out-of-band, never via caller headers. Updating a deployed
        allowlist remains an explicitly approved publisher/config operation.
        """
        current = self.head()
        if not current:
            return []
        pins = git(self.path, 'rev-list', '--first-parent', '--max-count=1025', current).decode().splitlines()
        if len(pins) > 1024:
            raise Refused('publication pin limit')
        return pins

    def resolve(self, pin):
        if not OID.fullmatch(pin):
            raise Refused('invalid catalog pin')
        current = self.head()
        if not current or git(self.path, 'merge-base', '--is-ancestor', pin, current, check=False).returncode:
            raise Refused('unpublished catalog pin')
        size = int(git(self.path, 'cat-file', '-s', pin + ':manifest.json'))
        if size > MAX_MANIFEST:
            raise Refused('manifest limit')
        return validate(git(self.path, 'show', pin + ':manifest.json'))


class FixtureAuthorization:
    """Loopback-only TEST authority. The header is NOT a production credential.

    Policy is loaded on every request, including fetch POST, for revocation.
    Grant scope is repository + source + exact state + policy epoch.
    """
    def __init__(self, path):
        self.path = Path(path)

    def authorize(self, principal, manifest):
        policy = json.loads(self.path.read_text())
        allowed = policy.get('readers', {}).get(principal, [])
        scope = [manifest[k] for k in ('repository', 'source', 'thread', 'state', 'policy_epoch')]
        if scope not in allowed:
            raise Refused('denied or stale policy epoch')


def thread_grants(authorized_threads):
    """Exact trusted local dependency grants, separate from caller-controlled manifests."""
    if authorized_threads is None:
        return {}
    if not isinstance(authorized_threads, dict) or len(authorized_threads) > 1024:
        raise Refused('invalid source Thread grants')
    grants = {}
    for source, names in authorized_threads.items():
        if (not isinstance(source, str) or not NAME.fullmatch(source) or not isinstance(names, list) or
            not 1 <= len(names) <= 128 or any(not isinstance(name, str) or len(name) > 255 or
                not THREAD.fullmatch(name) for name in names) or len(set(names)) != len(names)):
            raise Refused('invalid source Thread grants')
        grants[source] = tuple(names)
    return grants


class NativeSource:
    """Configured native repository lookup, never a path/URL supplied by a manifest."""
    def __init__(self, binary, sources, authorized_threads=None):
        self.binary = str(Path(binary).resolve())
        self.sources = {key: str(Path(value).resolve()) for key, value in sources.items()}
        self.authorized_threads = thread_grants(authorized_threads)

    def materialize(self, manifest, destination):
        source = self.sources.get(manifest['source'])
        if source is None:
            raise Refused('missing native source')
        allowed = self.authorized_threads.get(manifest['source'])
        if allowed is not None and manifest['thread'] not in allowed:
            raise Refused('governing Thread is not authorized')
        # The Rust helper caps states, depth, entries and uncompressed bytes before export.
        # Its fresh sink contains only the selected authorized closure, never native packs.
        env = git_env()
        env.update({'HEDDLE_PRINCIPAL_NAME': 'Synthetic Demo',
                    'HEDDLE_PRINCIPAL_EMAIL': 'demo@example.invalid'})
        metadata = Path(source) / '.heddle'
        if metadata.is_symlink() or not metadata.is_dir() or any((metadata / name).exists() for name in ('objectstore', 'lazy-hydrator.toml')):
            raise Refused('only complete local native fixtures supported')
        # Repository::open may reconcile local indexes. Run it on a private metadata
        # copy so the authoritative fixture remains byte-for-byte unchanged.
        count = size = 0
        for path in metadata.rglob('*'):
            count += 1
            metadata_stat = path.lstat()
            if count > 20000 or not (stat.S_ISDIR(metadata_stat.st_mode) or stat.S_ISREG(metadata_stat.st_mode)):
                raise Refused('native source layout limit')
            size += metadata_stat.st_size
            if size > MAX_DISK:
                raise Refused('native fixture size limit')
        with tempfile.TemporaryDirectory(prefix='heddle-native-read-') as temp:
            env['HEDDLE_HOME'] = str(Path(temp) / 'home')
            copy = Path(temp) / 'native'
            shutil.copytree(metadata, copy / '.heddle', ignore=shutil.ignore_patterns('identity.toml'))
            command = [self.binary, 'export', str(copy), manifest['state'], str(destination), manifest['mode'], manifest['thread']]
            if allowed is not None:
                command.append(json.dumps(allowed, separators=(',', ':')))
            p = bounded_process(command,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=30)
        if p.returncode:
            raise Refused('native source unavailable or projection refused')
        oid = p.stdout.decode().strip()
        if oid != manifest['git_oid']:
            raise Refused('source does not match pinned Git OID')
        size = sum(p.stat().st_size for p in Path(destination).rglob('*') if p.is_file())
        if size > MAX_DISK:
            raise Refused('materialized repository limit')
        git(destination, 'update-ref', 'refs/heads/main', oid)
        git(destination, 'symbolic-ref', 'HEAD', 'refs/heads/main')
        git(destination, 'config', 'http.receivepack', 'false')
        git(destination, 'config', 'uploadpack.allowAnySHA1InWant', 'false')
        git(destination, 'config', 'uploadpack.allowReachableSHA1InWant', 'false')
        git(destination, 'fsck', '--strict', '--no-reflogs')
