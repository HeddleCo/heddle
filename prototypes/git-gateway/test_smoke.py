# SPDX-License-Identifier: Apache-2.0
import hashlib
import io
import tarfile
import json
import os
from pathlib import Path
import tempfile
import unittest
from gateway.core import Refused
from gateway.smoke import inputs, run


class SmokeTests(unittest.TestCase):
    def test_retained_loopback_proof_is_explicitly_local(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / 'proof'
            report = run(Path(os.environ['GATEWAY_NATIVE']), out)
            self.assertEqual(report['status'], 'passed')
            self.assertFalse(report['live_cloudflare_verified'])
            self.assertEqual(report['external_service_requests'], 0)
            self.assertGreater(report['native_bundle_reads'], 0)
            self.assertIn('reader revocation before native bytes', report['checks'])
            self.assertIn('independent service revocation before native bytes', report['checks'])
            self.assertTrue((out / 'fresh-clone' / '.git').is_dir())
            self.assertFalse((out / 'fresh-clone' / '.heddle').exists())
            self.assertEqual(json.loads((out / 'report.json').read_text()), report)

    def test_existing_output_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileExistsError):
                run(Path(os.environ['GATEWAY_NATIVE']), Path(directory))

    def test_non_synthetic_dataset_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'DATA.json').write_text('{"synthetic_only":false,"authority_included":false}')
            with self.assertRaisesRegex(Refused, 'synthetic'):
                inputs('/unused', root, root)

    def test_old_native_format_refused_before_catalog_or_listener(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stream = io.BytesIO()
            with tarfile.open(fileobj=stream, mode='w') as archive:
                config = b'[repository]\nversion = 5\n'
                member = tarfile.TarInfo('.heddle/config.toml'); member.size = len(config)
                archive.addfile(member, io.BytesIO(config))
            data = stream.getvalue()
            (root / 'native.bundle').write_bytes(data)
            (root / 'DATA.json').write_text(json.dumps(dict(synthetic_only=True, authority_included=False,
                native_bundle_sha256=hashlib.sha256(data).hexdigest(), native_bundle_bytes=len(data))))
            with self.assertRaisesRegex(Refused, 'format v5.*v6'):
                inputs(Path(os.environ['GATEWAY_NATIVE']), root, root)
