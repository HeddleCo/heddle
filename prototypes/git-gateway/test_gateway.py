# SPDX-License-Identifier: Apache-2.0
import concurrent.futures
import hashlib
import http.client
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch

from gateway.core import LocalCatalog, NativeSource, FixtureAuthorization, Refused, git, git_env, validate
from gateway.artifacts import ArtifactsCatalog
from test_artifacts import RawFileResponse
from urllib.parse import urlsplit, parse_qs
from gateway.demo import setup
from gateway.http import server

BINARY = Path(os.environ['GATEWAY_NATIVE']).resolve()


def digest_tree(path):
    return {str(p.relative_to(path)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in path.rglob('*') if p.is_file()}


class GatewayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.temp.name) / 'demo'
        cls.evidence = setup(BINARY, cls.root)
        cls.config = cls.evidence['config']
        cls.native = Path(cls.config['sources']['native-demo'])
        cls.native_before = digest_tree(cls.native)
        cls.catalog = LocalCatalog(cls.config['catalog'])
        cls.source = NativeSource(BINARY, cls.config['sources'])
        cls.authority = FixtureAuthorization(cls.config['policy'])
        cls.app = server(cls.catalog, cls.source, cls.authority)
        cls.thread = threading.Thread(target=cls.app.serve_forever, daemon=True)
        cls.thread.start()
        cls.origin = f'http://127.0.0.1:{cls.app.server_port}'

    @classmethod
    def tearDownClass(cls):
        cls.app.shutdown(); cls.thread.join(); cls.app.server_close()
        if digest_tree(cls.native) != cls.native_before:
            raise AssertionError('gateway changed authoritative native fixture')
        cls.temp.cleanup()

    def view(self, name='merged'):
        return self.evidence['views'][name]

    def url(self, name='merged'):
        return self.origin + '/views/' + self.view(name)['pin'] + '.git'

    def request(self, suffix='/info/refs?service=git-upload-pack', reader='demo-reader', method='GET', body=None, headers=None):
        c = http.client.HTTPConnection('127.0.0.1', self.app.server_port, timeout=40)
        h = {'X-Demo-Reader': reader}; h.update(headers or {})
        c.request(method, '/views/' + self.view()['pin'] + '.git' + suffix, body=body, headers=h)
        r = c.getresponse(); status, data = r.status, r.read(); c.close()
        return status, data

    def test_clone_fetch_contents_fsck_and_catalog_separation(self):
        clone = self.root / 'clone'
        env = git_env()
        def client(*args):
            return subprocess.check_output(['git', '-c', 'http.extraHeader=X-Demo-Reader: demo-reader', *args], env=env, stderr=subprocess.STDOUT, timeout=60)
        client('clone', self.url('base'), str(clone))
        self.assertEqual((clone / 'README.md').read_text(), 'Synthetic native Heddle fixture\n')
        self.assertFalse((clone / 'agent-a.txt').exists())
        client('-C', str(clone), 'fetch', self.url(), 'refs/heads/main:refs/remotes/view/main')
        client('-C', str(clone), 'checkout', '--detach', 'refs/remotes/view/main')
        for name in ('agent-a', 'agent-b'):
            self.assertEqual((clone / (name + '.txt')).read_text(), f'Concurrent edit from {name}\n')
        client('-C', str(clone), 'fsck', '--strict')
        oid = client('-C', str(clone), 'rev-parse', 'HEAD').decode().strip()
        self.assertEqual(oid, self.view()['manifest']['git_oid'])
        self.assertNotEqual(oid, self.view()['pin'])
        self.assertEqual(client('-C', str(clone), 'rev-list', '--count', 'HEAD').strip(), b'1')
        self.assertEqual(git(self.catalog.path, 'ls-tree', '--name-only', self.view()['pin']), b'manifest.json\n')
        git(self.catalog.path, 'fsck', '--strict')
        # Both discovery protocol variants use native Git's implementation.
        for protocol in ('1', '2'):
            self.assertIn(oid.encode(), client('-c', 'protocol.version=' + protocol, 'ls-remote', self.url()))
        payload = b'Unpublished restricted fixture bytes\n'
        hidden_oid = hashlib.sha1(b'blob ' + str(len(payload)).encode() + b'\0' + payload).hexdigest()
        with self.assertRaises(subprocess.CalledProcessError):
            client('-C', str(clone), 'fetch', self.url(), hidden_oid)

        print('EVIDENCE clone/fetch/fsck:', json.dumps(self.evidence['views'], sort_keys=True))

    def test_deterministic_projection_and_native_threads(self):
        self.assertNotEqual(self.evidence['states']['a'], self.evidence['states']['b'])
        self.assertFalse((self.root / 'fixture' / 'agent-a' / 'agent-b.txt').exists())
        self.assertFalse((self.root / 'fixture' / 'agent-b' / 'agent-a.txt').exists())
        for i in range(2):
            with tempfile.TemporaryDirectory() as temp:
                dest = Path(temp) / 'view.git'
                self.source.materialize(self.view()['manifest'], dest)
                self.assertEqual(git(dest, 'rev-parse', 'HEAD').decode().strip(), self.view()['manifest']['git_oid'])
                self.assertEqual(set(git(dest, 'for-each-ref', '--format=%(refname)').splitlines()), {b'refs/heads/main'})

    def test_ordered_merge_parents_and_hidden_objects(self):
        with tempfile.TemporaryDirectory() as temp:
            destination = Path(temp) / 'history.git'
            self.source.materialize(self.view('ordered')['manifest'], destination)
            oid = self.view('ordered')['manifest']['git_oid']
            parents = git(destination, 'rev-list', '--parents', '-n', '1', oid).decode().split()[1:]
            self.assertEqual(len(parents), 2)
            # The ordered merge is [derived view, base], both admitted on main.
            first_message = git(destination, 'show', '-s', '--format=%B', parents[0])
            second_message = git(destination, 'show', '-s', '--format=%B', parents[1])
            self.assertIn(b'derived concurrent view', first_message)
            self.assertTrue(second_message.startswith(b'base'))
            payload = b'Unpublished restricted fixture bytes\n'
            hidden_oid = hashlib.sha1(b'blob ' + str(len(payload)).encode() + b'\0' + payload).hexdigest()
            self.assertNotEqual(git(destination, 'cat-file', '-e', hidden_oid, check=False).returncode, 0)
            git(destination, 'fsck', '--strict')
        restricted = dict(self.view()['manifest'], state=self.evidence['states']['restricted'])
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(Refused, 'projection refused'):
                self.source.materialize(restricted, Path(temp) / 'restricted.git')

    def test_denied_and_revoked_request_does_not_materialize(self):
        with patch.object(self.source, 'materialize', wraps=self.source.materialize) as materialize:
            status, _ = self.request(reader='untrusted')
            self.assertEqual(status, 403)
            materialize.assert_not_called()
        path = Path(self.config['policy']); before = path.read_text()
        try:
            path.write_text('{"readers":{}}')
            self.assertEqual(self.request()[0], 403)
        finally:
            path.write_text(before)

    def test_write_paths_and_noncanonical_routes_rejected(self):
        self.assertEqual(self.request('/info/refs?service=git-receive-pack')[0], 405)
        self.assertEqual(self.request('/git-receive-pack', method='POST')[0], 404)
        self.assertEqual(self.request('/../other.git/info/refs?service=git-upload-pack')[0], 404)
        self.assertEqual(self.request('/info%2frefs?service=git-upload-pack')[0], 404)
        self.assertEqual(self.request('/git-upload-pack', method='POST', headers={'Content-Length': '1048577'})[0], 413)
        self.assertEqual(self.request('/git-upload-pack', method='POST', headers={'Content-Encoding': 'gzip'})[0], 400)

    def test_missing_corrupt_source_and_stale_manifest(self):
        manifest = dict(self.view()['manifest'])
        with tempfile.TemporaryDirectory() as temp:
            manifest['git_oid'] = 'a' * 40
            with self.assertRaisesRegex(Refused, 'does not match'):
                self.source.materialize(manifest, Path(temp) / 'bad.git')
            manifest = dict(self.view()['manifest'], source='unknown')
            with self.assertRaisesRegex(Refused, 'missing native'):
                self.source.materialize(manifest, Path(temp) / 'missing.git')
            manifest = dict(self.view()['manifest'], state='hs-' + '0' * 52)
            with self.assertRaisesRegex(Refused, 'projection refused'):
                self.source.materialize(manifest, Path(temp) / 'missing-state.git')
            bad = Path(temp) / 'bad-source'; shutil.copytree(self.native, bad)
            # Corrupt native config is a real malformed-source refusal, never served as a partial repo.
            (bad / '.heddle' / 'config.toml').write_text('not valid = [')
            with self.assertRaisesRegex(Refused, 'projection refused'):
                NativeSource(BINARY, {'native-demo': bad}).materialize(self.view()['manifest'], Path(temp) / 'corrupt.git')
            corrupt = Path(temp) / 'corrupt-packs'; shutil.copytree(self.native, corrupt)
            packs = list((corrupt / '.heddle' / 'packs').glob('*.pack'))
            self.assertTrue(packs, 'real native pack corruption case must not be vacuous')
            for pack in packs:
                pack.write_bytes(b'corrupted synthetic native pack')
            with self.assertRaisesRegex(Refused, 'projection refused'):
                NativeSource(BINARY, {'native-demo': corrupt}).materialize(self.view()['manifest'], Path(temp) / 'corrupt-pack-view.git')
        with self.assertRaisesRegex(Refused, 'stale'):
            self.authority.authorize('demo-reader', dict(self.view()['manifest'], policy_epoch=2))

    def test_concurrent_publication_and_lost_ack_recovery(self):
        with tempfile.TemporaryDirectory() as temp:
            catalog = LocalCatalog(Path(temp) / 'catalog.git')
            initial = catalog.publish(self.view('base')['manifest'])
            barrier = threading.Barrier(2)
            def publish(epoch):
                barrier.wait()
                try:
                    return catalog.publish(dict(self.view()['manifest'], policy_epoch=epoch), initial)
                except Refused:
                    return None
            with concurrent.futures.ThreadPoolExecutor(2) as pool:
                results = list(pool.map(publish, (2, 3)))
            winners = [r for r in results if r]
            self.assertEqual(len(winners), 1)
            current = catalog.resolve(winners[0])
            self.assertEqual(catalog.publish(current, initial), winners[0])
            advanced = catalog.publish(dict(current, policy_epoch=4), winners[0])
            self.assertEqual(catalog.publish(current, initial), winners[0])
            self.assertEqual(catalog.head(), advanced)
            with self.assertRaises(Refused): catalog.resolve('f' * 40)
            unreachable = git(catalog.path, 'fsck', '--unreachable', '--no-reflogs').decode()
            losers = [line.split()[2] for line in unreachable.splitlines() if line.startswith('unreachable commit ')]
            self.assertTrue(losers, 'CAS losing candidate must be exercised')
            for loser in losers:
                with self.assertRaises(Refused): catalog.resolve(loser)

    def test_artifacts_adapter_substitutes_for_catalog_in_real_git_clone_LOCAL_FIXTURES(self):
        seen = []
        def documented_rest_fixture(request, timeout):
            seen.append(request.full_url)
            query = parse_qs(urlsplit(request.full_url).query)
            self.assertEqual(query['path'], ['manifest.json'])
            # Faithful raw REST bytes backed by the real local catalog. This is
            # deliberately a response fixture, NOT a call to a Cloudflare account.
            return RawFileResponse(git(self.catalog.path, 'show', query['ref'][0] + ':manifest.json'))
        catalog = ArtifactsCatalog('0' * 32, 'default', 'catalog', self.catalog.published_pins(),
                                  'fixture-only-not-a-credential', open_request=documented_rest_fixture)
        app = server(catalog, self.source, self.authority)
        thread = threading.Thread(target=app.serve_forever, daemon=True); thread.start()
        try:
            with tempfile.TemporaryDirectory() as temp:
                subprocess.run(['git', '-c', 'http.extraHeader=X-Demo-Reader: demo-reader', 'clone',
                    f'http://127.0.0.1:{app.server_port}/views/{self.view()["pin"]}.git', temp],
                    env=git_env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, timeout=30)
                self.assertEqual((Path(temp) / 'agent-a.txt').read_text(), 'Concurrent edit from agent-a\n')
                git(Path(temp) / '.git', 'fsck', '--strict')
            self.assertGreaterEqual(len(seen), 2)
            self.assertEqual(catalog.resolve(self.view()['pin']), self.catalog.resolve(self.view()['pin']))
        finally:
            app.shutdown(); thread.join(); app.server_close()

    def test_fixture_server_rejects_remote_origin_and_proxy_wiring(self):
        self.assertEqual(self.app.server_address[0], '127.0.0.1')
        for headers in ({'Host': 'gateway.example.invalid'}, {'Origin': 'https://example.invalid'},
                        {'Authorization': 'Bearer fixture'}, {'Forwarded': 'for=192.0.2.1'},
                        {'X-Forwarded-For': '192.0.2.1'}, {'X-Forwarded-Host': 'gateway.example.invalid'}):
            with self.subTest(headers=headers): self.assertEqual(self.request(headers=headers)[0], 403)
        for length in ('+1', '-1', '1,2'):
            self.assertEqual(self.request('/git-upload-pack', method='POST', headers={'Content-Length': length})[0], 400)

    def test_process_environment_and_cgi_headers_cannot_redirect_git(self):
        injected = {'GIT_DIR': '/nonexistent', 'GIT_WORK_TREE': '/nonexistent',
                    'GIT_OBJECT_DIRECTORY': '/nonexistent', 'GIT_ALTERNATE_OBJECT_DIRECTORIES': '/nonexistent',
                    'GIT_EXEC_PATH': '/nonexistent', 'GIT_CONFIG_COUNT': '1',
                    'GIT_CONFIG_KEY_0': 'uploadpack.hideRefs', 'GIT_CONFIG_VALUE_0': 'refs/heads/main'}
        with patch.dict(os.environ, injected):
            status, body = self.request(headers={'GIT_PROJECT_ROOT': '/nonexistent', 'PATH_INFO': '/private.git/info/refs',
                                                'Git-Protocol': 'version=2:invalid=value'})
        self.assertEqual(status, 200)
        self.assertIn(self.view()['manifest']['git_oid'].encode(), body)

    def test_projection_does_not_need_git_or_worktree_caches(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); native = root / 'native'; shutil.copytree(self.native, native)
            cache = native / '.heddle' / 'git-projection'; cache.mkdir(exist_ok=True)
            (cache / 'git-projection-mapping.json').write_text('invalid old Git mapping')
            (native / '.git').mkdir(); (native / '.git' / 'HEAD').write_text('not a Git repository')
            source = NativeSource(BINARY, {'native-demo': native})
            source.materialize(self.view()['manifest'], root / 'poisoned-cache-view.git')
            for path in (cache, native / '.git', native / '.heddle' / 'state', native / '.heddle' / 'materialized-roots'):
                if path.exists(): shutil.rmtree(path)
            source.materialize(self.view()['manifest'], root / 'cacheless-view.git')
            self.assertEqual(git(root / 'cacheless-view.git', 'rev-parse', 'HEAD').decode().strip(), self.view()['manifest']['git_oid'])
            # The catalog is independently readable even without any native copy.
            shutil.rmtree(native)
            self.assertEqual(self.catalog.resolve(self.view()['pin']), self.view()['manifest'])

    def test_native_symlinks_refused_before_copy(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); native = root / 'native'; shutil.copytree(self.native, native)
            outside = root / 'outside'; outside.write_text('synthetic outside file')
            (native / '.heddle' / 'outside-link').symlink_to(outside)
            with self.assertRaisesRegex(Refused, 'layout limit'):
                NativeSource(BINARY, {'native-demo': native}).materialize(self.view()['manifest'], root / 'view.git')
            self.assertFalse((root / 'view.git').exists())

    def test_publication_advance_during_resolution_cannot_switch_pinned_manifest(self):
        with tempfile.TemporaryDirectory() as temp:
            catalog = LocalCatalog(Path(temp) / 'catalog.git')
            pin = catalog.publish(self.view('base')['manifest'])
            writer = LocalCatalog(catalog.path)
            read_head = threading.Event(); advance_done = threading.Event()
            original_head = catalog.head
            def paused_head():
                head = original_head(); read_head.set()
                self.assertTrue(advance_done.wait(10)); return head
            with patch.object(catalog, 'head', side_effect=paused_head), concurrent.futures.ThreadPoolExecutor(1) as pool:
                pending = pool.submit(catalog.resolve, pin)
                self.assertTrue(read_head.wait(10))
                writer.publish(self.view()['manifest'], pin); advance_done.set()
                self.assertEqual(pending.result(timeout=10), self.view('base')['manifest'])

    def test_manifest_validation(self):
        for data in (b'{}', b'[]', b'x' * 16385):
            with self.assertRaises(Refused): validate(data)
        for changed in ({'source': '../private'}, {'git_oid': 'HEAD'}, {'policy_epoch': True}):
            with self.assertRaises(Refused): validate(json.dumps(dict(self.view()['manifest'], **changed)).encode())

if __name__ == '__main__':
    unittest.main(verbosity=2)
