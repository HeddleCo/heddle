# SPDX-License-Identifier: Apache-2.0
"""Local synthetic proof of host composition, never a live cloud integration."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
from gateway.auth import ServiceAuthorization, read_json
from gateway.core import Refused, bounded_process, canonical, git_env, thread_grants
from gateway.demo import setup
from gateway.host import configured_server
from gateway.native_bundle import NativeBundleSource, pack_fixture

# Public test vectors only. No generated/deployed credential or account is used.
READER = 'PUBLIC_TEST_VECTOR_READER_NOT_SECRET_0000000'
SERVICE = 'PUBLIC_TEST_VECTOR_SERVICE_NOT_SECRET_000000'


class HostTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.temp.name)
        cls.binary = Path(os.environ['GATEWAY_NATIVE']).resolve()
        cls.evidence = setup(cls.binary, cls.root / 'demo')
        bundle = pack_fixture(cls.evidence['config']['sources']['native-demo'])
        cls.bundle = cls.root / 'native.bundle'
        cls.bundle.write_bytes(bundle)
        cls.policy = cls.root / 'readers.json'
        cls.services = cls.root / 'services.json'
        views = cls.evidence['views']
        cls.pin = views['merged']['pin']
        cls.manifest = views['merged']['manifest']
        cls.reader_document = {'readers': [{'sha256': hashlib.sha256(READER.encode()).hexdigest(),
            'expires_at': 4102444800, 'views': [[v['manifest'][k] for k in
            ('repository', 'source', 'thread', 'state', 'policy_epoch')] for v in views.values()]}]}
        cls.service_document = {'services': [{'sha256': hashlib.sha256(SERVICE.encode()).hexdigest(),
            'expires_at': 4102444800, 'pins': [v['pin'] for v in views.values()]}]}
        cls.policy.write_text(json.dumps(cls.reader_document))
        cls.services.write_text(json.dumps(cls.service_document))
        cls.config = {'schema': 1, 'binary': str(cls.binary), 'port': 0,
            'reader_policy': str(cls.policy), 'service_policy': str(cls.services),
            'catalog': {'kind': 'local', 'path': cls.evidence['config']['catalog']},
            'native': {'kind': 'local-bundles', 'descriptors': {'native-demo': hashlib.sha256(bundle).hexdigest()},
                'paths': {'native-demo': str(cls.bundle)}}}

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def setUp(self):
        self.policy.write_text(json.dumps(self.reader_document))
        self.services.write_text(json.dumps(self.service_document))
        self.app = configured_server(self.config)
        self.thread = threading.Thread(target=self.app.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.app.shutdown(); self.thread.join(); self.app.server_close()

    def request(self, *, reader=READER, service=SERVICE, extra=None, method='GET'):
        headers = {'Authorization': 'Bearer ' + reader}
        if service is not None:
            headers['X-Gateway-Service-Authorization'] = 'Bearer ' + service
        headers.update(extra or {})
        endpoint = '/info/refs?service=git-upload-pack'
        if method == 'POST':
            endpoint = '/git-upload-pack'
            headers['Content-Type'] = 'application/x-git-upload-pack-request'
        connection = http.client.HTTPConnection('127.0.0.1', self.app.server_port, timeout=40)
        connection.request(method, f'/views/{self.pin}.git' + endpoint,
            body=b'0000' if method == 'POST' else None, headers=headers)
        response = connection.getresponse()
        status = response.status
        response.read(); connection.close()
        return status

    def test_service_authority_precedes_catalog_and_reader_is_independent(self):
        with patch('gateway.core.LocalCatalog.resolve', side_effect=AssertionError('must not read catalog')):
            self.assertEqual(self.request(service=None), 403)
            self.assertEqual(self.request(service=READER), 403)
            self.services.write_text('{"services": []}')
            self.assertEqual(self.request(), 403)
        self.services.write_text(json.dumps(self.service_document))
        with patch('gateway.native_bundle.NativeBundleSource.materialize', side_effect=AssertionError('must not read source')):
            self.assertEqual(self.request(reader=SERVICE), 403)
            self.policy.write_text('{"readers": []}')
            self.assertEqual(self.request(), 403)
            self.assertEqual(self.request(method='POST'), 403)

    def test_ordinary_git_clone_and_both_authorities_revoke(self):
        url = f'http://127.0.0.1:{self.app.server_port}/views/{self.pin}.git'
        command = ['git', '-c', 'http.extraHeader=Authorization: Bearer ' + READER,
            '-c', 'http.extraHeader=X-Gateway-Service-Authorization: Bearer ' + SERVICE]
        with tempfile.TemporaryDirectory() as directory:
            clone = Path(directory) / 'clone'
            subprocess.run(command + ['clone', url, str(clone)], env=git_env(), capture_output=True, check=True, timeout=60)
            for name in ('agent-a', 'agent-b'):
                self.assertEqual((clone / (name + '.txt')).read_text(), f'Concurrent edit from {name}\n')
            subprocess.run(['git', '-C', str(clone), 'fsck', '--strict'], env=git_env(), capture_output=True, check=True)
            self.services.write_text('{"services": []}')
            self.assertNotEqual(subprocess.run(command + ['ls-remote', url], env=git_env(), capture_output=True).returncode, 0)
            self.services.write_text(json.dumps(self.service_document))
            self.policy.write_text('{"readers": []}')
            self.assertNotEqual(subprocess.run(command + ['ls-remote', url], env=git_env(), capture_output=True).returncode, 0)

    def test_fixture_proxy_and_malformed_authority_fail_closed(self):
        for headers in [{'X-Demo-Reader': 'demo-reader'}, {'Forwarded': 'for=127.0.0.1'}, {'Origin': 'https://untrusted.invalid'}]:
            self.assertEqual(self.request(extra=headers), 403)
        self.services.write_text('{"services": [], "services": []}')
        self.assertEqual(self.request(), 403)
        self.services.write_text('[]')
        self.assertEqual(self.request(), 403)

    def test_configuration_has_no_fixture_or_public_bind_fallback(self):
        for change in [{'bind': '0.0.0.0'}, {'schema': True}, {'native': {'kind': 'fixture'}},
                       {'catalog': {'kind': 'local', 'path': str(self.root / 'missing')}}]:
            with self.assertRaises(Refused):
                configured_server(dict(self.config, **change))

    def test_worker_transport_to_real_host_clone_fetch_and_revocation_LOCAL_FIXTURES(self):
        fixture_path = self.root / 'worker-fixture.json'
        fixture_path.write_text(json.dumps({'port': self.app.server_port,
            'manifests': {v['pin']: json.dumps(v['manifest'], sort_keys=True, separators=(',', ':')) + '\n'
                for v in self.evidence['views'].values()}}))
        process = subprocess.Popen(['node', str(Path(__file__).parent / 'worker/local-host-smoke.mjs'), str(fixture_path)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            ready, _, _ = select.select([process.stdout], [], [], 10)
            self.assertTrue(ready, 'local Worker startup deadline')
            line = process.stdout.readline()
            self.assertRegex(line, r'LOCAL_FIXTURE_WORKER_PORT=\d+')
            origin = 'http://127.0.0.1:' + line.strip().split('=')[1]
            def url(name): return origin + '/views/' + self.evidence['views'][name]['pin'] + '.git'
            command = ['git', '-c', 'http.extraHeader=Authorization: Bearer ' + READER,
                '-c', 'http.extraHeader=X-Gateway-Service-Authorization: attacker-supplied',
                '-c', 'http.extraHeader=X-Demo-Reader: forged-fixture']
            with tempfile.TemporaryDirectory() as directory:
                clone = Path(directory) / 'clone'
                subprocess.run(command + ['clone', url('base'), str(clone)], env=git_env(), capture_output=True, check=True, timeout=60)
                self.assertFalse((clone / 'agent-a.txt').exists())
                subprocess.run(command + ['-C', str(clone), 'fetch', url('merged'), 'refs/heads/main:refs/remotes/view/main'],
                    env=git_env(), capture_output=True, check=True, timeout=60)
                subprocess.run(['git', '-C', str(clone), 'checkout', '--detach', 'refs/remotes/view/main'],
                    env=git_env(), capture_output=True, check=True)
                for name in ('agent-a', 'agent-b'):
                    self.assertEqual((clone / (name + '.txt')).read_text(), f'Concurrent edit from {name}\n')
                subprocess.run(['git', '-C', str(clone), 'fsck', '--strict'], env=git_env(), capture_output=True, check=True)
                self.services.write_text('{"services": []}')
                self.assertNotEqual(subprocess.run(command + ['ls-remote', url('merged')], env=git_env(), capture_output=True).returncode, 0)
        finally:
            process.terminate()
            process.communicate(timeout=10)

    @unittest.skipUnless(sys.platform.startswith('linux') and os.geteuid() != 0, 'non-root Linux host')
    def test_actual_host_entrypoint_starts_with_runtime_limits(self):
        config_path = self.root / 'host.json'
        config_path.write_text(json.dumps(self.config))
        env = dict(os.environ, PYTHONPATH=str(Path(__file__).parent), PYTHONDONTWRITEBYTECODE='1')
        process = subprocess.Popen([sys.executable, '-m', 'gateway.host', str(config_path)],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            ready, _, _ = select.select([process.stdout], [], [], 10)
            self.assertTrue(ready, 'host startup deadline')
            line = process.stdout.readline()
            self.assertRegex(line, r'Authenticated internal gateway on 127\.0\.0\.1:\d+')
            limits = Path(f'/proc/{process.pid}/limits').read_text()
            self.assertIn('1610612736', limits)
            port = int(line.strip().rsplit(':', 1)[1])
            connection = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
            connection.request('GET', f'/views/{self.pin}.git/info/refs?service=git-upload-pack')
            response = connection.getresponse()
            self.assertEqual(response.status, 403)
            response.read(); connection.close()
        finally:
            process.terminate()
            process.communicate(timeout=10)


class AuthorityBoundsTests(unittest.TestCase):
    def test_nested_thread_names_and_explicit_grants_are_bounded(self):
        manifest = dict(schema=1, repository='demo', source='native', thread='demo/priority-sort',
            state='hs-' + 'a' * 52, git_oid='a' * 40, mode='snapshot', policy_epoch=1)
        self.assertEqual(json.loads(canonical(manifest))['thread'], 'demo/priority-sort')
        for name in ['../main', 'demo/../main', 'demo//main', '/main', 'demo%2fmain', 'demo\\main', 'a' * 256]:
            with self.assertRaises(Refused): canonical(dict(manifest, thread=name))
            with self.assertRaises(Refused): thread_grants({'native': [name]})
        names = ['main', 'demo/priority-sort']
        grants = thread_grants({'native': names})
        names.append('unapproved')
        self.assertEqual(grants['native'], ('main', 'demo/priority-sort'))
        for bad in [{'native': []}, {'native': ['main', 'main']}, {'native': 'main'}, {'native': ['main'] * 129}]:
            with self.assertRaises(Refused): thread_grants(bad)
        def must_not_read(_): raise AssertionError('unlisted Thread must not read source bytes')
        source = NativeBundleSource('/unused', {'native': 'a' * 64}, must_not_read,
            {'native': ['main']})
        with self.assertRaisesRegex(Refused, 'governing Thread'):
            source.materialize(manifest, '/unused')

    @unittest.skipUnless(sys.platform.startswith('linux') and os.geteuid() != 0, 'non-root Linux host')
    def test_runtime_respects_stricter_inherited_container_limits(self):
        code = ('import resource,sys,subprocess; from gateway.host import runtime_limits; '
            'from gateway.core import bounded_process,git_env; '
            'resource.setrlimit(resource.RLIMIT_NOFILE,(64,64)); runtime_limits(); '
            'assert resource.getrlimit(resource.RLIMIT_NOFILE)==(64,64); '
            'p=bounded_process([sys.executable,"-c","import resource; assert resource.getrlimit(resource.RLIMIT_NOFILE)==(64,64)"], '
            'env=git_env(),stdout=subprocess.PIPE,stderr=subprocess.PIPE); assert p.returncode==0,p.stderr')
        result = subprocess.run([sys.executable, '-c', code],
            env=dict(os.environ, PYTHONPATH=str(Path(__file__).parent)), capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_policy_rejects_ambiguity_oversize_and_nonfinite_expiry(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'policy.json'
            for raw in ['[]', '{"readers": [], "readers": []}', ' ' * 65537]:
                path.write_text(raw)
                with self.assertRaises(Refused): read_json(path)
            grant = {'sha256': hashlib.sha256(SERVICE.encode()).hexdigest(), 'expires_at': float('inf'), 'pins': ['a' * 40]}
            path.write_text(json.dumps({'services': [grant]}))
            with self.assertRaises(Refused): ServiceAuthorization(path).authorize('Bearer ' + SERVICE, 'a' * 40)
            grant['expires_at'] = 4102444800
            path.write_text(json.dumps({'services': [grant, grant]}))
            with self.assertRaises(Refused): ServiceAuthorization(path).authorize('Bearer ' + SERVICE, 'a' * 40)

    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux RLIMIT_AS')
    def test_native_children_have_hard_memory_and_no_core_dump_limit(self):
        code = ('import resource; '
            'assert resource.getrlimit(resource.RLIMIT_AS)==(1073741824,1073741824); '
            'assert resource.getrlimit(resource.RLIMIT_CORE)==(0,0); '
            'x=bytearray(2*1024*1024*1024)')
        result = bounded_process([sys.executable, '-c', code], env=git_env(),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'MemoryError', result.stderr)
