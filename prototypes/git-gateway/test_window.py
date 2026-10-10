# SPDX-License-Identifier: Apache-2.0
"""Real Git smart-HTTP proof for the explicit, loopback-only native window.

No hosted source, account, deployment, or remote repository is used. The fixture
helper creates a genuine native repository and signed baseline capture. Git
clients use only disposable local identities and public test-vector tokens.

Run after building gateway_host and gateway_agent_demo:
  python3 prototypes/git-gateway/test_window.py -v
Optional environment variables: GATEWAY_HOST, GATEWAY_AGENT_DEMO,
GATEWAY_WINDOW_BASELINE (public six-file demo tree), GATEWAY_WINDOW_PORT
(18780..18789), and GATEWAY_WINDOW_EVIDENCE (a new output directory). Without an
external baseline the test writes its own tiny public app.js fixture.
"""
import hashlib
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[2]
# Deliberately public test vectors; never generated or deployed credentials.
READER = 'PUBLIC_TEST_VECTOR_WINDOW_READER_NOT_SECRET_0000'
WRITER = 'PUBLIC_TEST_VECTOR_WINDOW_WRITER_NOT_SECRET_0000'
SERVICE = 'PUBLIC_TEST_VECTOR_WINDOW_SERVICE_NOT_SECRET_000'
REVOKED = 'PUBLIC_TEST_VECTOR_WINDOW_REVOKED_NOT_SECRET_000'


def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(',', ':')) + '\n').encode()


def digest(value):
    return hashlib.sha256(value.encode()).hexdigest()


def packet(value):
    return f'{len(value) + 4:04x}'.encode() + value


class NativeWindowHttpTests(unittest.TestCase):
    """Each case owns a fresh native store, catalog, identity and Git clients."""

    @classmethod
    def setUpClass(cls):
        target = Path(os.environ.get('CARGO_TARGET_DIR', REPO / 'target'))
        cls.host_binary = Path(os.environ.get('GATEWAY_HOST', target / 'debug/examples/gateway_host')).resolve()
        cls.fixture_binary = Path(os.environ.get('GATEWAY_AGENT_DEMO', target / 'debug/examples/gateway_agent_demo')).resolve()
        for binary in (cls.host_binary, cls.fixture_binary):
            if not binary.is_file():
                raise RuntimeError(f'Build the native gateway examples first: {binary}')
        cls.evidence_root = None
        if os.environ.get('GATEWAY_WINDOW_EVIDENCE'):
            cls.evidence_root = Path(os.environ['GATEWAY_WINDOW_EVIDENCE']).resolve()
            cls.evidence_root.mkdir(parents=True, exist_ok=False)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='heddle-window-http-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.process = None
        self.log = []
        self.facts = {}
        self.addCleanup(self.save_evidence)
        self.addCleanup(self.stop_host)
        self.native = self.root / 'fixture/native'
        baseline = os.environ.get('GATEWAY_WINDOW_BASELINE')
        if baseline:
            baseline = Path(baseline).resolve()
            self.assertFalse((baseline / '.heddle').exists(), 'only public baseline source may be copied')
            self.assertFalse((baseline / '.git').exists(), 'baseline must not be a Git checkout')
            shutil.copytree(baseline, self.native)
        else:
            self.native.mkdir(parents=True)
            (self.native / 'app.js').write_text('export const features = [];\n')
        self.home = self.root / 'home'
        self.home.mkdir()
        self.tempdir = self.root / 'host-temporaries'
        self.tempdir.mkdir()
        # Do not inherit Git redirects, credentials, configuration, or Heddle identity.
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(('GIT_', 'HEDDLE_')) and key not in ('HOME', 'XDG_CONFIG_HOME', 'TMPDIR')}
        self.env.update(HOME=str(self.home), XDG_CONFIG_HOME=str(self.home / '.config'),
                        HEDDLE_HOME=str(self.root / 'identity'),
                        HEDDLE_PRINCIPAL_NAME='Synthetic Window Test',
                        HEDDLE_PRINCIPAL_EMAIL='window@example.invalid',
                        GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
                        GIT_TERMINAL_PROMPT='0', GIT_AUTHOR_NAME='Synthetic Git Client',
                        GIT_AUTHOR_EMAIL='git-client@example.invalid',
                        GIT_COMMITTER_NAME='Synthetic Git Client',
                        GIT_COMMITTER_EMAIL='git-client@example.invalid',
                        GIT_AUTHOR_DATE='2026-01-01T00:00:00+00:00',
                        GIT_COMMITTER_DATE='2026-01-01T00:00:00+00:00',
                        LC_ALL='C', NO_PROXY='127.0.0.1,localhost')
        prepared = self.run_command([self.fixture_binary, 'prepare', self.native.parent])
        self.facts['native_fixture'] = json.loads(prepared.stdout)
        self.catalog = self.root / 'catalog'
        self.config_path = self.root / 'window.json'
        self.config = dict(schema=1, scope='quiescent-synthetic-local',
                           expires_at=int(time.time()) + 1800, repository='synthetic-window',
                           thread='main', native=str(self.native), catalog=str(self.catalog),
                           reader_sha256=digest(READER), writer_sha256=digest(WRITER),
                           service_sha256=digest(SERVICE), actor='synthetic-writer',
                           policy_generation=digest('local-window-generation-one'))
        self.write_config()
        self.port = self.available_port()
        self.url = f'http://127.0.0.1:{self.port}/repositories/synthetic-window.git'
        self.start_host()
        self.baseline = self.publication()
        self.baseline_native = self.native_snapshot()
        self.assertTrue(self.baseline_native['source_operations'], 'genuine signed baseline capture required')
        self.assertTrue(all(op['signature_bytes'] == 64 for op in self.baseline_native['source_operations']))
        self.assertEqual(self.baseline_native['heads'], [bytes(self.baseline['native_state']).hex().upper()])
        self.assert_quarantine_empty()

    def save_evidence(self):
        if self.evidence_root:
            out = self.evidence_root / self._testMethodName
            out.mkdir()
            (out / 'commands.json').write_bytes(canonical(self.log))
            (out / 'facts.json').write_bytes(canonical(self.facts))
            for path in sorted(self.root.glob('host-*.log')):
                shutil.copyfile(path, out / path.name)

    def available_port(self):
        requested = os.environ.get('GATEWAY_WINDOW_PORT')
        ports = [int(requested)] if requested else range(18780, 18790)
        for port in ports:
            self.assertTrue(18780 <= port <= 18789, 'local window test port must be in 18780..18789')
            with socket.socket() as probe:
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                try:
                    probe.bind(('127.0.0.1', port))
                    return port
                except OSError:
                    continue
        self.fail('No free loopback port in the explicit local test range')

    def run_command(self, args, *, data=None, succeeds=True):
        args = [str(value) for value in args]
        result = subprocess.run(args, input=data, env=self.env, capture_output=True, timeout=60)
        row = dict(argv=args, returncode=result.returncode,
                   stdout=result.stdout.decode(errors='replace'), stderr=result.stderr.decode(errors='replace'))
        # Binary pack bodies are not useful evidence; preserve their exact digest.
        if '\x00' in row['stdout'] or result.stdout.startswith(b'PACK'):
            row['stdout'] = '<binary output>'
            row['stdout_sha256'] = hashlib.sha256(result.stdout).hexdigest()
        self.log.append(row)
        if succeeds:
            self.assertEqual(result.returncode, 0, json.dumps(row, indent=2))
        else:
            self.assertNotEqual(result.returncode, 0, json.dumps(row, indent=2))
        return result

    def git(self, *args, writer=False, reader=READER, service=SERVICE, succeeds=True, data=None):
        command = ['git', '-c', 'credential.helper=', '-c', 'http.proxy=',
                   '-c', 'http.extraHeader=Authorization: Bearer ' + reader,
                   '-c', 'http.extraHeader=X-Gateway-Service-Authorization: Bearer ' + service]
        if writer:
            command += ['-c', 'http.extraHeader=X-Gateway-Write-Authorization: Bearer ' + WRITER]
        return self.run_command(command + list(args), data=data, succeeds=succeeds)

    def start_host(self):
        index = len(list(self.root.glob('host-*.log')))
        self.host_log = self.root / f'host-{index}.log'
        with self.host_log.open('wb') as stream:
            self.process = subprocess.Popen([str(self.host_binary), '--local-window', str(self.config_path),
                                             '--bind', f'127.0.0.1:{self.port}'],
                                            env=dict(self.env, TMPDIR=str(self.tempdir)), stdout=stream, stderr=stream)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.fail(f'Native window exited before listening:\n{self.host_log.read_text()}')
            if 'Local synthetic Git window listening at ' in self.host_log.read_text():
                return
            time.sleep(0.05)
        self.fail(f'Native window startup deadline:\n{self.host_log.read_text()}')

    def stop_host(self):
        if self.process is not None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
            self.process = None

    def write_config(self):
        temporary = self.config_path.with_suffix('.new')
        temporary.write_bytes(canonical(self.config))
        temporary.replace(self.config_path)

    def publication(self):
        return json.loads((self.catalog / 'current.json').read_bytes())

    def native_snapshot(self):
        # Read-only inspection of this test's synthetic authoritative transaction.
        path = self.native / '.heddle/metadata.sqlite3'
        with sqlite3.connect(path.as_uri() + '?mode=ro', uri=True) as db:
            operations = [dict(operation=oid, revision=revision, signature_bytes=size)
                          for oid, revision, size in db.execute(
                              "SELECT hex(o.id),hex(o.source_revision),length(o.signature) FROM operations o "
                              "JOIN thread_list t ON o.thread=t.thread "
                              "WHERE t.name='main' AND o.status=1 AND o.facet=1 ORDER BY o.id")]
            heads = [row[0] for row in db.execute(
                "SELECT hex(h.revision) FROM thread_source_head_revisions h "
                "JOIN thread_list t ON h.thread=t.thread WHERE t.name='main' ORDER BY h.revision")]
            receipts = [json.loads(row[0]) for row in db.execute(
                "SELECT response FROM operation_receipts WHERE verb='LocalGitPush' ORDER BY operation_id")]
        return dict(source_operations=operations, heads=heads, receipts=receipts)

    def clone(self, name):
        clone = self.root / name
        self.git('clone', self.url, clone)
        self.assertFalse((clone / '.git/shallow').exists())
        self.assertFalse((clone / '.heddle').exists(), 'native metadata must not enter the Git worktree')
        refs = self.git('-C', clone, 'for-each-ref', '--format=%(refname)').stdout.decode().splitlines()
        self.assertEqual(refs, ['refs/heads/main', 'refs/remotes/origin/HEAD', 'refs/remotes/origin/main'])
        return clone

    def commit(self, clone, name, content=None):
        (clone / name).write_text(content or f'Synthetic window edit: {name}\n')
        self.git('-C', clone, 'add', '--', name)
        self.git('-C', clone, 'commit', '-m', f'Add {name}')
        return self.git('-C', clone, 'rev-parse', 'HEAD').stdout.decode().strip()

    def history(self, clone):
        return self.git('-C', clone, 'rev-list', '--reverse', 'HEAD').stdout.decode().splitlines()

    def assert_clone_identity(self, clone, expected, history):
        self.assertEqual(self.git('-C', clone, 'rev-parse', 'HEAD').stdout.decode().strip(), expected)
        self.assertEqual(self.history(clone), history)
        self.git('-C', clone, 'fsck', '--strict')
        self.assertFalse((clone / '.git/shallow').exists())
        self.assertEqual(self.git('-C', clone, 'status', '--porcelain').stdout, b'')

    def assert_quarantine_empty(self):
        # Requests release temporary projections/quarantine before the next request.
        deadline = time.monotonic() + 3
        while list(self.tempdir.iterdir()) and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertEqual(list(self.tempdir.iterdir()), [], 'quarantine or projection leaked')

    def assert_unchanged(self, publication, native):
        self.assertEqual(self.publication(), publication)
        self.assertEqual(self.native_snapshot(), native)
        self.assert_quarantine_empty()

    def receive_body(self, clone, old, new, *, commands=None):
        pack = self.git('-C', clone, 'pack-objects', '--stdout', '--revs', data=f'{new}\n^{old}\n'.encode()).stdout
        if commands is None:
            commands = [(old, new, 'refs/heads/main')]
        body = b''
        for i, (prior, following, ref) in enumerate(commands):
            line = f'{prior} {following} {ref}'.encode()
            if i == 0:
                line += b'\0report-status ofs-delta object-format=sha1'
            body += packet(line)
        return body + b'0000' + pack

    def http(self, method='GET', *, service='git-upload-pack', body=None, writer=False):
        headers = {'Authorization': 'Bearer ' + READER,
                   'X-Gateway-Service-Authorization': 'Bearer ' + SERVICE}
        if writer:
            headers['X-Gateway-Write-Authorization'] = 'Bearer ' + WRITER
        endpoint = f'/info/refs?service={service}'
        if method == 'POST':
            endpoint = '/' + service
            headers['Content-Type'] = f'application/x-{service}-request'
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=60)
        try:
            connection.request(method, '/repositories/synthetic-window.git' + endpoint,
                               body=body, headers=headers)
            response = connection.getresponse()
            payload = response.read()
            self.log.append(dict(http_method=method, endpoint=endpoint, status=response.status,
                                 response=payload.decode(errors='replace') if response.status != 200 else '<Git protocol>',
                                 request_sha256=hashlib.sha256(body or b'').hexdigest()))
            return response.status, payload
        finally:
            connection.close()

    def test_clone_two_commits_push_fresh_clone_fetch_pull_and_restart(self):
        author = self.clone('author')
        second = self.clone('second-client')
        original = self.history(author)
        first = self.commit(author, 'first-git-edit.txt')
        tip = self.commit(author, 'second-git-edit.txt')
        expected = original + [first, tip]
        replay = self.receive_body(author, self.baseline['git_oid'], tip)
        self.git('-C', author, 'push', 'origin', 'main', writer=True)
        published = self.publication()
        accepted = self.native_snapshot()
        self.assertEqual(published['mode'], 'full-history')
        self.assertEqual(published['git_oid'], tip)
        self.assertEqual(len(published['receipt']['operations']), 2)
        self.assertEqual(len(accepted['source_operations']), len(self.baseline_native['source_operations']) + 2)
        self.assertEqual(len(accepted['receipts']), 1)
        self.assertEqual(self.http('POST', service='git-receive-pack', body=replay, writer=True)[0], 200)
        self.assert_unchanged(published, accepted)
        fresh = self.clone('fresh-reader')
        self.assert_clone_identity(fresh, tip, expected)
        for oid in expected:
            self.assertEqual(self.git('-C', author, 'cat-file', 'commit', oid).stdout,
                             self.git('-C', fresh, 'cat-file', 'commit', oid).stdout)
        for name in ('first-git-edit.txt', 'second-git-edit.txt'):
            self.assertEqual((author / name).read_bytes(), (fresh / name).read_bytes())
        self.git('-C', second, 'fetch', 'origin')
        self.assertEqual(self.git('-C', second, 'rev-parse', 'origin/main').stdout.decode().strip(), tip)
        self.git('-C', second, 'pull', '--ff-only')
        self.assert_clone_identity(second, tip, expected)
        self.assert_quarantine_empty()
        self.stop_host()
        self.start_host()
        restarted = self.clone('after-restart')
        self.assert_clone_identity(restarted, tip, expected)
        self.assert_unchanged(published, accepted)
        self.facts.update(exact_git_tip=tip, full_history=expected, accepted=accepted,
                          commit_bytes_preserved=True, successful_post_replay_idempotent=True, second_client_fetch_pull=True,
                          restart_persisted=True, quarantine_discarded=True)

    def test_reader_cannot_push_or_substitute_for_writer_or_service(self):
        author = self.clone('reader')
        old = self.baseline['git_oid']
        tip = self.commit(author, 'reader-must-not-write.txt')
        body = self.receive_body(author, old, tip)
        self.git('-C', author, 'push', 'origin', 'main', succeeds=False)
        self.assertEqual(self.http(service='git-receive-pack')[0], 403)
        self.assertEqual(self.http('POST', service='git-receive-pack', body=body)[0], 403)
        self.git('ls-remote', self.url, reader=WRITER, succeeds=False)
        self.git('ls-remote', self.url, service=READER, succeeds=False)
        self.assert_unchanged(self.baseline, self.baseline_native)
        self.facts['reader_and_writer_and_service_independent'] = True

    def test_force_delete_create_and_multiref_are_server_refused(self):
        author = self.clone('author')
        original = self.baseline['git_oid']
        tip = self.commit(author, 'accepted.txt')
        self.git('-C', author, 'push', 'origin', 'main', writer=True)
        published, accepted = self.publication(), self.native_snapshot()
        following = self.commit(author, 'unaccepted.txt')
        for name, refspecs in [('force', ['--force', f'{original}:refs/heads/main']),
                               ('delete', [':refs/heads/main']),
                               ('create', ['HEAD:refs/heads/new-branch']),
                               ('multiref', ['HEAD:refs/heads/main', 'HEAD:refs/heads/new-branch'])]:
            with self.subTest(shape=name):
                result = self.git('-C', author, 'push', 'origin', *refspecs, writer=True, succeeds=False)
                if name == 'delete':
                    # Ordinary Git respects the server's omission of delete-refs.
                    self.assertIn(b'[remote rejected]', result.stderr)
                else:
                    self.assertIn(b'403', result.stderr)
                self.assert_unchanged(published, accepted)
        # Direct envelopes also prove server rejection, independent of client
        # heuristics (notably Git honoring the omitted delete-refs capability).
        commands = {
            'force': [(tip, original, 'refs/heads/main')],
            'delete': [(tip, '0' * 40, 'refs/heads/main')],
            'create': [('0' * 40, following, 'refs/heads/new-branch')],
            'multiref': [(tip, following, 'refs/heads/main'),
                         ('0' * 40, following, 'refs/heads/other')],
        }
        for name, updates in commands.items():
            with self.subTest(raw_shape=name):
                body = self.receive_body(author, tip, following, commands=updates)
                self.assertEqual(self.http('POST', service='git-receive-pack', body=body, writer=True)[0], 403)
                self.assert_unchanged(published, accepted)
        self.facts['rejected_shapes'] = ['force', 'delete', 'create', 'multiref']

    def test_stale_concurrent_client_and_prepared_receive_are_refused(self):
        left, right = self.clone('left'), self.clone('right')
        old = self.baseline['git_oid']
        winner = self.commit(left, 'left.txt')
        stale = self.commit(right, 'right.txt')
        body = self.receive_body(right, old, stale)
        self.git('-C', left, 'push', 'origin', 'main', writer=True)
        published, accepted = self.publication(), self.native_snapshot()
        self.git('-C', right, 'push', 'origin', 'main', writer=True, succeeds=False)
        self.assertEqual(self.http('POST', service='git-receive-pack', body=body, writer=True)[0], 403)
        self.assert_unchanged(published, accepted)
        self.assertEqual(published['git_oid'], winner)
        self.facts.update(winner=winner, stale_tip=stale, prepared_stale_receive_denied=True)

    def test_malformed_pack_is_refused_without_native_acceptance_or_quarantine(self):
        author = self.clone('author')
        tip = self.commit(author, 'corrupt-pack.txt')
        valid = self.receive_body(author, self.baseline['git_oid'], tip)
        malformed = valid[:-1] + bytes([valid[-1] ^ 1])
        self.assertEqual(self.http('POST', service='git-receive-pack', body=malformed, writer=True)[0], 403)
        self.assert_unchanged(self.baseline, self.baseline_native)
        self.facts.update(malformed_pack_denied=True, native_acceptance_unchanged=True, quarantine_discarded=True)

    def test_reader_writer_and_service_revocation_rechecked_for_each_request(self):
        author = self.clone('author')
        tip = self.commit(author, 'revoked.txt')
        body = self.receive_body(author, self.baseline['git_oid'], tip)
        for key in ('writer_sha256', 'reader_sha256', 'service_sha256'):
            with self.subTest(revoked=key):
                previous = self.config[key]
                self.config[key] = digest(REVOKED)
                self.write_config()
                try:
                    expected_read = 200 if key == 'writer_sha256' else 403
                    self.assertEqual(self.http()[0], expected_read)
                    self.assertEqual(self.http(service='git-receive-pack', writer=True)[0], 403)
                    self.assertEqual(self.http('POST', service='git-receive-pack', body=body, writer=True)[0], 403)
                    self.git('-C', author, 'push', 'origin', 'main', writer=True, succeeds=False)
                    self.assert_unchanged(self.baseline, self.baseline_native)
                finally:
                    self.config[key] = previous
                    self.write_config()
        self.git('ls-remote', self.url)
        self.facts['rechecked_revocations'] = ['reader', 'writer', 'service']

    def test_durable_acceptance_publication_failure_retry_recovers_without_duplicates(self):
        author = self.clone('author')
        history = self.history(author)
        tip = self.commit(author, 'publication-recovery.txt')
        history.append(tip)
        marker = self.catalog / 'publication-paused'
        marker.write_text('explicit local synthetic fault injection\n')
        failed = self.git('-C', author, 'push', 'origin', 'main', writer=True, succeeds=False)
        self.assertIn(b'403', failed.stderr)
        self.assertEqual(self.publication(), self.baseline, 'publication must not advance while paused')
        pending_native = self.native_snapshot()
        self.assertEqual(len(pending_native['receipts']), 1, 'native receipt must be durable despite HTTP failure')
        self.assertEqual(pending_native['receipts'][0]['new_git'], tip)
        self.assertEqual(len(pending_native['source_operations']), len(self.baseline_native['source_operations']) + 1)
        self.assertNotEqual(pending_native['heads'], self.baseline_native['heads'])
        self.assertEqual(self.http()[0], 403, 'readers must not see an older catalog over newer native acceptance')
        self.git('ls-remote', self.url, succeeds=False)
        self.assert_quarantine_empty()
        # Restart while unpublished: recovery must use persisted native receipt,
        # not a cached pack, in-memory transaction, or surviving quarantine.
        self.stop_host()
        self.start_host()
        self.assertEqual(self.native_snapshot(), pending_native)
        self.assertEqual(self.http()[0], 403)
        self.assertEqual(self.http(service='git-receive-pack', writer=True)[0], 403)
        self.assertEqual(self.publication(), self.baseline)
        self.assertEqual(self.native_snapshot(), pending_native)
        marker.unlink()
        retry = self.git('-C', author, 'push', 'origin', 'main', writer=True)
        self.assertIn(b'Everything up-to-date', retry.stderr)
        published = self.publication()
        self.assertEqual(published['git_oid'], tip)
        self.assertEqual(published['receipt'], pending_native['receipts'][0])
        self.assertEqual(self.native_snapshot(), pending_native, 'retry must not append another source or receipt')
        self.assertEqual(len(list((self.catalog / 'versions').glob('*.json'))), 2)
        self.assert_clone_identity(self.clone('recovered-reader'), tip, history)
        self.stop_host()
        self.start_host()
        self.assert_clone_identity(self.clone('restarted-reader'), tip, history)
        self.assert_unchanged(published, pending_native)
        self.facts.update(exact_git_tip=tip, full_history=history, accepted=pending_native,
                          http_failure_after_durable_acceptance=True, reads_denied_until_publication=True,
                          retry_discovery_recovered=True, retry_reported_up_to_date=True,
                          recovery_survived_restart=True, duplicated_history=False, quarantine_discarded=True)


if __name__ == '__main__':
    unittest.main()
