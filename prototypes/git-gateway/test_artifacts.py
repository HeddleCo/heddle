# SPDX-License-Identifier: Apache-2.0
"""LOCAL response-fixture contract tests. No Artifacts account/network access."""
from contextlib import closing
from email.message import Message
import io
import json
from pathlib import Path
import unittest
from gateway.artifacts import ArtifactsCatalog, NoRedirect
from gateway.core import Refused, validate

CONTRACT = json.loads(Path(__file__).with_name('catalog-fixtures.json').read_text())

class RawFileResponse(io.BytesIO):
    """Documented REST /file response: raw bytes, not an API JSON envelope."""
    def __init__(self, data, status=200, headers=None):
        super().__init__(data)
        self.status = status
        self.headers = Message()
        self.headers['Content-Type'] = 'application/octet-stream'
        for key, value in (headers or {}).items(): self.headers[key] = value


def adapter(open_request, pins=None):
    return ArtifactsCatalog('0' * 32, 'default', 'catalog', pins or [CONTRACT['pin']],
                            'fixture-only-not-a-credential', open_request=open_request)

class ArtifactsContractTests(unittest.TestCase):
    def test_documented_read_url_raw_response_and_closed_handle(self):
        response = RawFileResponse(CONTRACT['valid'].encode())
        seen = []
        def read(request, timeout):
            seen.append((request, timeout)); return response
        self.assertEqual(adapter(read).resolve(CONTRACT['pin']), validate(CONTRACT['valid'].encode()))
        request, timeout = seen[0]
        self.assertEqual(request.get_method(), 'GET')
        self.assertEqual(request.full_url, 'https://api.cloudflare.com/client/v4/accounts/' + '0' * 32 +
                         '/artifacts/namespaces/default/repos/catalog/file?ref=' + CONTRACT['pin'] + '&path=manifest.json')
        self.assertEqual(timeout, 10)
        self.assertTrue(response.closed)

    def test_shared_contract(self):
        for invalid in CONTRACT['invalid']:
            with self.subTest(invalid=invalid), self.assertRaises(Refused): validate(invalid.encode())

    def test_unpublished_pin_no_io_and_allowlist_is_immutable(self):
        seen = []
        pins = [CONTRACT['pin']]
        reader = adapter(lambda *args, **kw: seen.append(args), pins)
        pins.append('c' * 40)
        for bad in ('HEAD', '../escape', 'c' * 40):
            with self.assertRaises(Refused): reader.resolve(bad)
        self.assertEqual(seen, [])

    def test_missing_malformed_oversized_encoded_responses(self):
        for response in [RawFileResponse(b'', status=404), RawFileResponse(b'{}'),
                         RawFileResponse(b'x' * 16385), RawFileResponse(CONTRACT['valid'].encode(), headers={'Content-Length': '999999'}),
                         RawFileResponse(CONTRACT['valid'].encode(), headers={'Content-Encoding': 'gzip'})]:
            with self.subTest(response=response), self.assertRaises(Refused):
                adapter(lambda *args, **kw: response).resolve(CONTRACT['pin'])
            self.assertTrue(response.closed)

    def test_redirect_and_configuration_injection_refused(self):
        with self.assertRaises(Refused): NoRedirect().redirect_request(None, None, None, None, None, None)
        for namespace in ('../escape', 'a?token=bad', 'a\r\nHeader: value'):
            with self.assertRaises(Refused):
                ArtifactsCatalog('0' * 32, namespace, 'catalog', [], 'fixture')
