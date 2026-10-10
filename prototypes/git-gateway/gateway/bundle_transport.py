# SPDX-License-Identifier: Apache-2.0
"""HTTPS transport for a separately authenticated internal R2 bundle service."""
import re
import urllib.error
import urllib.request
from urllib.parse import urlsplit
from .core import NAME, Refused
from .native_bundle import MAX_BUNDLE

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None

class HTTPSBundleReader:
    def __init__(self, origin, credential, opener=None):
        parsed = urlsplit(origin)
        if (parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password or
            parsed.path not in ('', '/') or parsed.query or parsed.fragment):
            raise ValueError('fixed HTTPS service origin required')
        if not re.fullmatch(r'[A-Za-z0-9_-]{32,256}', credential):
            raise ValueError('pre-provisioned service credential required')
        self.origin, self.credential = origin.rstrip('/'), credential
        self.opener = opener or urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def __call__(self, source):
        if not isinstance(source, str) or not NAME.fullmatch(source):
            raise Refused('invalid source name')
        request = urllib.request.Request(self.origin + '/native/' + source,
            headers={'Authorization': 'Bearer ' + self.credential, 'Accept': 'application/octet-stream'})
        try:
            with self.opener.open(request, timeout=15) as response:
                if response.status != 200 or response.headers.get('Content-Type') != 'application/octet-stream':
                    raise Refused('native transport unavailable')
                if response.headers.get('Content-Encoding'):
                    raise Refused('encoded native bundle refused')
                length = response.headers.get('Content-Length')
                if length is not None and (not length.isascii() or not length.isdecimal() or int(length) > MAX_BUNDLE):
                    raise Refused('native transport limit')
                data = response.read(MAX_BUNDLE + 1)
                if len(data) > MAX_BUNDLE or length is not None and len(data) != int(length):
                    raise Refused('native transport limit')
                return data
        except (OSError, urllib.error.URLError, ValueError):
            raise Refused('native transport unavailable') from None
