# SPDX-License-Identifier: Apache-2.0
"""Real read-only Artifacts REST adapter; no network is used by its fixture tests.

Same resolve(pin)->manifest contract as LocalCatalog. Publication membership is a
trusted immutable allowlist exported by the local publisher, NOT object existence.
No token creation, remote writes, native data, redirects or environment proxies.
"""
import re
from urllib.parse import quote, urlencode
from urllib.request import Request, HTTPRedirectHandler, ProxyHandler, build_opener
from .core import MAX_MANIFEST, OID, Refused, validate


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise Refused('Artifacts redirects refused')


class ArtifactsCatalog:
    def __init__(self, account, namespace, repository, published_pins, bearer, open_request=None):
        if not re.fullmatch(r'[0-9a-f]{32}', account):
            raise Refused('invalid configured account')
        for name in (namespace, repository):
            if not re.fullmatch(r'[a-zA-Z0-9_-]{1,128}', name):
                raise Refused('invalid configured catalog name')
        if not isinstance(published_pins, (list, tuple, frozenset)) or len(published_pins) > 1024:
            raise Refused('publication pin limit')
        if any(not isinstance(pin, str) or not OID.fullmatch(pin) for pin in published_pins):
            raise Refused('invalid published pin')
        self.published_pins = frozenset(published_pins)
        if not bearer or '\r' in bearer or '\n' in bearer:
            raise Refused('an existing authorized read credential is required')
        self._bearer = bearer
        self._base = (f'https://api.cloudflare.com/client/v4/accounts/{account}/artifacts/'
                      f'namespaces/{quote(namespace, safe="")}/repos/{quote(repository, safe="")}/file')
        self._open = open_request or build_opener(ProxyHandler({}), NoRedirect()).open

    def resolve(self, pin):
        if not isinstance(pin, str) or not OID.fullmatch(pin) or pin not in self.published_pins:
            raise Refused('unpublished catalog pin')
        request = Request(self._base + '?' + urlencode({'ref': pin, 'path': 'manifest.json'}),
                          headers={'Authorization': 'Bearer ' + self._bearer,
                                   'Accept': 'application/octet-stream'}, method='GET')
        try:
            with self._open(request, timeout=10) as response:
                if response.status != 200 or response.headers.get('Content-Encoding', 'identity') != 'identity':
                    raise Refused('Artifacts read unavailable')
                length = response.headers.get('Content-Length')
                if length is not None and (not length.isdecimal() or int(length) > MAX_MANIFEST):
                    raise Refused('manifest limit')
                data = response.read(MAX_MANIFEST + 1)
        except Refused:
            raise
        except (OSError, ValueError):
            raise Refused('Artifacts read unavailable') from None
        return validate(data)
