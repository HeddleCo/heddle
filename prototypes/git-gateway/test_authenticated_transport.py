# SPDX-License-Identifier: Apache-2.0
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from gateway.auth import BearerAuthorization
from gateway.bundle_transport import HTTPSBundleReader
from gateway.core import LocalCatalog, Refused, git_env
from gateway.demo import setup
from gateway.http import server
from gateway.native_bundle import NativeBundleSource, pack_fixture

# Public test vector, never a deployed or generated credential.
TOKEN = 'PUBLIC_TEST_VECTOR_NOT_A_SECRET_0000000000'

class Response(io.BytesIO):
    status = 200
    def __init__(self, data):
        super().__init__(data)
        self.headers = {'Content-Type': 'application/octet-stream', 'Content-Length': str(len(data))}

class AuthenticatedTransportTests(unittest.TestCase):
    def test_https_native_transport_bearer_git_clone_and_revocation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = Path(os.environ['GATEWAY_NATIVE']).resolve()
            evidence = setup(binary, root / 'demo')
            config = evidence['config']
            bundle = pack_fixture(config['sources']['native-demo'])
            manifest = evidence['views']['merged']['manifest']
            scope = [manifest[k] for k in ('repository', 'source', 'thread', 'state', 'policy_epoch')]
            policy = root / 'bearer-policy.json'
            policy.write_text(json.dumps({'readers': [{'sha256': hashlib.sha256(TOKEN.encode()).hexdigest(), 'expires_at': 200, 'views': [scope]}]}))
            authority = BearerAuthorization(policy, clock=lambda: 100)
            test = self
            class Opener:
                def open(self, request, timeout):
                    test.assertEqual(request.full_url, 'https://internal.invalid/native/native-demo')
                    test.assertEqual(request.get_header('Authorization'), 'Bearer ' + TOKEN)
                    test.assertEqual(timeout, 15)
                    return Response(bundle)
            transport = HTTPSBundleReader('https://internal.invalid', TOKEN, opener=Opener())
            source = NativeBundleSource(binary, {'native-demo': hashlib.sha256(bundle).hexdigest()}, transport)
            app = server(LocalCatalog(config['catalog']), source, authority, bearer_mode=True)
            thread = threading.Thread(target=app.serve_forever, daemon=True); thread.start()
            try:
                url = f'http://127.0.0.1:{app.server_port}/views/' + evidence['views']['merged']['pin'] + '.git'
                command = ['git', '-c', 'http.extraHeader=Authorization: Bearer ' + TOKEN]
                clone = str(root / 'clone')
                subprocess.run(command + ['clone', url, clone], env=git_env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, timeout=60)
                subprocess.run(['git', '-C', clone, 'fsck', '--strict'], env=git_env(), capture_output=True, check=True)
                policy.write_text('{"readers": []}')
                result = subprocess.run(command + ['ls-remote', url], env=git_env(), capture_output=True, timeout=60)
                self.assertNotEqual(result.returncode, 0)
            finally:
                app.shutdown(); thread.join(); app.server_close()

    def test_authority_expiry_scope_and_malformed_credentials(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'policy.json'
            manifest = dict(repository='r', source='s', thread='t', state='state', policy_epoch=1)
            scope = list(manifest.values())
            document = {'readers': [{'sha256': hashlib.sha256(TOKEN.encode()).hexdigest(), 'expires_at': 100, 'views': [scope]}]}
            path.write_text(json.dumps(document))
            with self.assertRaises(Refused): BearerAuthorization(path, lambda:100).authorize('Bearer '+TOKEN, manifest)
            manifest['policy_epoch'] = 2
            with self.assertRaises(Refused): BearerAuthorization(path, lambda:99).authorize('Bearer '+TOKEN, manifest)
            for value in ['', 'Basic abc', 'Bearer short', 'Bearer '+TOKEN+'\n']:
                with self.assertRaises(Refused): BearerAuthorization(path).authorize(value, manifest)

    def test_transport_rejects_bad_origin_and_bad_response(self):
        for origin in ['http://internal.invalid', 'https://user:pass@internal.invalid', 'https://internal.invalid/path']:
            with self.assertRaises(ValueError): HTTPSBundleReader(origin, TOKEN)
        class Opener:
            def open(self, *args, **kwargs):
                response = Response(b'bad'); response.headers['Content-Length'] = '800000000'
                return response
        reader = HTTPSBundleReader('https://internal.invalid', TOKEN, opener=Opener())
        with self.assertRaises(Refused): reader('native-demo')
        with self.assertRaises(Refused): reader('../escape')
