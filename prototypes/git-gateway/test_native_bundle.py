# SPDX-License-Identifier: Apache-2.0
import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import threading
import unittest
from gateway.native_bundle import pack_fixture, unpack_fixture, NativeBundleSource
from gateway.core import LocalCatalog, FixtureAuthorization, Refused, git_env
from gateway.demo import setup
from gateway.http import server

class BundleTests(unittest.TestCase):
    def test_cold_clone_fetch_after_native_origin_removed(self):
        binary = Path(os.environ['GATEWAY_NATIVE']).resolve()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            evidence = setup(binary, root / 'demo')
            config = evidence['config']
            native = Path(config['sources']['native-demo'])
            data = pack_fixture(native)
            self.assertEqual(data, pack_fixture(native))
            with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                self.assertFalse(any('identity.toml' in name or 'git-projection' in name for name in archive.getnames()))
            shutil.rmtree(native)
            calls = []
            def read(source):
                calls.append(source)
                return data
            source = NativeBundleSource(binary, {'native-demo': hashlib.sha256(data).hexdigest()}, read)
            app = server(LocalCatalog(config['catalog']), source, FixtureAuthorization(config['policy']))
            thread = threading.Thread(target=app.serve_forever, daemon=True)
            thread.start()
            try:
                def client(*args):
                    return subprocess.check_output(['git', '-c', 'http.extraHeader=X-Demo-Reader: demo-reader', *args], env=git_env(), stderr=subprocess.STDOUT, timeout=60)
                def url(name):
                    return f'http://127.0.0.1:{app.server_port}/views/' + evidence['views'][name]['pin'] + '.git'
                clone = str(root / 'clone')
                client('clone', url('base'), clone)
                client('-C', clone, 'fetch', url('merged'), 'refs/heads/main:refs/remotes/view/main')
                client('-C', clone, 'checkout', '--detach', 'refs/remotes/view/main')
                client('-C', clone, 'fsck', '--strict')
                self.assertEqual(client('-C', clone, 'rev-parse', 'HEAD').decode().strip(), evidence['views']['merged']['manifest']['git_oid'])
                for agent in ('agent-a', 'agent-b'):
                    self.assertEqual((Path(clone) / (agent + '.txt')).read_text(), f'Concurrent edit from {agent}\n')
                self.assertGreaterEqual(len(calls), 4)
                self.assertFalse(native.exists())
            finally:
                app.shutdown(); thread.join(); app.server_close()

    def test_bad_digest_and_missing_source(self):
        for descriptors in ({}, {'native-demo': '0' * 64}):
            with self.assertRaises(Refused):
                NativeBundleSource('/unused', descriptors, lambda _: b'corrupt').materialize({'source': 'native-demo'}, '/unused')

    def test_unsafe_archive_entries(self):
        for name, kind in [('../escape', tarfile.REGTYPE), ('.heddle/identity.toml', tarfile.REGTYPE),
                           ('.heddle/link', tarfile.SYMTYPE), ('/.heddle/file', tarfile.REGTYPE)]:
            stream = io.BytesIO()
            with tarfile.open(fileobj=stream, mode='w') as archive:
                info = tarfile.TarInfo(name); info.type = kind; info.linkname = '/tmp/escape'
                archive.addfile(info, io.BytesIO())
            with tempfile.TemporaryDirectory() as temp, self.assertRaises(Refused):
                unpack_fixture(stream.getvalue(), temp)
