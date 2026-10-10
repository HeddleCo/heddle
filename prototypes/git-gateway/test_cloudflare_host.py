# SPDX-License-Identifier: Apache-2.0
"""Cloudflare-profile code contracts only; no actual platform or image execution."""
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
from gateway.cloudflare_bridge import CloudflareBindingBridge, ORIGIN
from gateway.cloudflare_host import ingress, native_server, validate_config
from gateway.core import Refused, canonical, git_env
from gateway.demo import setup
from gateway.native_bundle import pack_fixture

READER = 'PUBLIC_TEST_VECTOR_READER_NOT_SECRET_0000000'
SERVICE = 'PUBLIC_TEST_VECTOR_SERVICE_NOT_SECRET_000000'


class Response(io.BytesIO):
    status = 200
    def __init__(self, data):
        super().__init__(data)
        self.headers = {'Content-Type': 'application/octet-stream', 'Content-Length': str(len(data))}


class CloudflareHostTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.temp.name)
        cls.binary = Path(os.environ['GATEWAY_NATIVE']).resolve()
        evidence = setup(cls.binary, cls.root / 'demo')
        bundle = pack_fixture(evidence['config']['sources']['native-demo'])
        cls.bundle = bundle
        cls.views = {v['pin']: v['manifest'] for label, v in evidence['views'].items() if label in ('base', 'merged')}
        cls.pin = evidence['views']['merged']['pin']
        cls.config = {'schema': 1, 'expires_at': int(time.time()) + 600,
            'published_pins': sorted(cls.views),
            'reader_sha256': hashlib.sha256(READER.encode()).hexdigest(),
            'service_sha256': hashlib.sha256(SERVICE.encode()).hexdigest(),
            'views': [cls.views[pin] for pin in sorted(cls.views)],
            'descriptors': {'native-demo': hashlib.sha256(bundle).hexdigest()},
            'authorized_threads': {'native-demo': ['main']}}

    @classmethod
    def tearDownClass(cls): cls.temp.cleanup()

    def setUp(self):
        self.work = tempfile.TemporaryDirectory()
        self.calls = []
        test = self
        class Opener:
            def open(self, request, timeout):
                test.assertEqual(timeout, 15)
                test.assertIsNone(request.get_header('Authorization'))
                test.calls.append(request.full_url)
                if request.full_url == ORIGIN + '/native/native-demo': return Response(test.bundle)
                for pin, manifest in test.views.items():
                    if request.full_url == ORIGIN + '/catalog/' + pin: return Response(canonical(manifest))
                raise AssertionError('unexpected bridge request')
        self.bridge = CloudflareBindingBridge(self.views, self.config['descriptors'], opener=Opener())
        self.native, authority = native_server(self.config, self.binary, self.work.name, port=0, bridge=self.bridge)
        self.front = ingress(self.native.server_port, authority, bind='127.0.0.1', port=0)
        self.threads = [threading.Thread(target=app.serve_forever, daemon=True) for app in (self.native, self.front)]
        for thread in self.threads: thread.start()

    def tearDown(self):
        for app in (self.front, self.native): app.shutdown(); app.server_close()
        for thread in self.threads: thread.join()
        self.work.cleanup()

    def request(self, *, host='native-container.invalid', service=SERVICE, path=None, headers=None):
        supplied = {'Host': host, 'Authorization': 'Bearer ' + READER}
        if service is not None: supplied['X-Gateway-Service-Authorization'] = 'Bearer ' + service
        supplied.update(headers or {})
        connection = http.client.HTTPConnection('127.0.0.1', self.front.server_port, timeout=40)
        connection.request('GET', path or f'/views/{self.pin}.git/info/refs?service=git-upload-pack', headers=supplied)
        response = connection.getresponse(); status = response.status; response.read(); connection.close()
        return status

    def test_ingress_reader_and_service_identity_before_native_bytes(self):
        self.assertEqual(self.request(service=None), 403)
        self.assertEqual(self.request(service=READER), 403)
        self.assertEqual(self.request(host='untrusted.invalid'), 403)
        self.assertEqual(self.calls, [])
        self.assertEqual(self.request(headers={'Origin': 'https://untrusted.invalid'}), 403)
        self.assertEqual(self.request(headers={'X-Demo-Reader': 'fixture'}), 403)
        self.assertEqual(self.request(path='/native/native-demo'), 404)
        self.assertEqual(self.calls, [])

    def test_actual_loopback_ingress_git_clone_and_independent_reader_revocation(self):
        url = f'http://127.0.0.1:{self.front.server_port}/views/{self.pin}.git'
        command = ['git', '-c', 'http.extraHeader=Host: native-container.invalid',
            '-c', 'http.extraHeader=Authorization: Bearer ' + READER,
            '-c', 'http.extraHeader=X-Gateway-Service-Authorization: Bearer ' + SERVICE]
        clone = Path(self.work.name) / 'clone'
        result = subprocess.run(command + ['clone', url, str(clone)], env=git_env(), capture_output=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((clone / 'agent-a.txt').read_text(), 'Concurrent edit from agent-a\n')
        self.assertEqual((clone / 'agent-b.txt').read_text(), 'Concurrent edit from agent-b\n')
        subprocess.run(['git', '-C', str(clone), 'fsck', '--strict'], env=git_env(), capture_output=True, check=True)
        (Path(self.work.name) / 'readers.json').write_text('{"readers": []}')
        self.assertEqual(self.request(), 403)
        (Path(self.work.name) / 'services.json').write_text('{"services": []}')
        before = len(self.calls)
        self.assertEqual(self.request(), 403)
        self.assertEqual(len(self.calls), before)

    def test_bridge_refuses_unlisted_or_changed_manifest(self):
        with self.assertRaises(Refused): self.bridge.resolve('f' * 40)
        with self.assertRaises(Refused): self.bridge.read_bundle('../native-demo')
        self.assertEqual(self.calls, [])
        class Opener:
            def open(inner, *args, **kwargs):
                return Response(canonical(dict(self.views[self.pin], policy_epoch=2)))
        bad = CloudflareBindingBridge(self.views, self.config['descriptors'], opener=Opener())
        with self.assertRaisesRegex(Refused, 'differs'): bad.resolve(self.pin)

    def test_config_requires_short_expiry_distinct_digests_and_exact_scope(self):
        for patch in [{'expires_at': 0}, {'expires_at': int(time.time()) + 901},
                      {'service_sha256': self.config['reader_sha256']}, {'schema': True},
                      {'published_pins': ['f' * 40] * 3}, {'authorized_threads': {}},
                      {'extra': 'unrecognized'}]:
            with self.assertRaises(Refused): validate_config(dict(self.config, **patch))

    def test_worker_snapshot_matches_native_configuration_contract(self):
        policy = {'schema': 1, 'issued_at': int(time.time()) - 1, 'expires_at': self.config['expires_at'],
            'catalog': 'demo-catalog', 'pins': self.views,
            'readers': [{'sha256': self.config['reader_sha256'], 'expires_at': self.config['expires_at'],
                'views': [[m[k] for k in ('repository', 'source', 'thread', 'state', 'policy_epoch')] for m in self.config['views']]}],
            'gateway_service': {'sha256': self.config['service_sha256'], 'expires_at': self.config['expires_at'], 'pins': self.config['published_pins']},
            'bridge': {'expires_at': self.config['expires_at'], 'pins': self.config['published_pins'], 'sources': ['native-demo']},
            'sources': {'native-demo': {'sha256': self.config['descriptors']['native-demo'],
                'key': 'native/' + self.config['descriptors']['native-demo'] + '.bundle', 'authorized_threads': ['main']}}}
        root = Path(__file__).parent / 'worker/cloudflare'
        code = ('import {readFileSync} from "node:fs"; '
            'import {canonicalJson,readPolicy} from "./policy.mjs"; '
            'import {nativeConfig} from "./composition.mjs"; '
            'console.log(JSON.stringify(nativeConfig(readPolicy(canonicalJson(JSON.parse(readFileSync(0,"utf8")))))));')
        result = subprocess.run(['node', '--input-type=module', '-e', code], cwd=root,
            input=json.dumps(policy), text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        snapshot = json.loads(result.stdout)
        self.assertEqual(validate_config(snapshot), self.config)

    def test_bridge_bounds_actual_and_declared_bytes(self):
        class Opener:
            def open(inner, *args, **kwargs):
                response = Response(b'x' * 16385)
                response.headers['Content-Length'] = '10'
                return response
        bad = CloudflareBindingBridge(self.views, self.config['descriptors'], opener=Opener())
        with self.assertRaises(Refused): bad.resolve(self.pin)
