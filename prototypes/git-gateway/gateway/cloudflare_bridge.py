# SPDX-License-Identifier: Apache-2.0
"""Explicit Cloudflare-only outbound bridge, not a general HTTP transport fallback.

Cloudflare documents this fixed-host, same-machine outbound channel as encrypted
by its networking stack. The Worker authenticates platform-supplied Container ID,
exact pin/source and expiry. No raw outgoing credential enters Linux.
"""
import urllib.error
import urllib.request
from .bundle_transport import NoRedirect
from .core import MAX_MANIFEST, NAME, OID, Refused, canonical, validate
from .native_bundle import MAX_BUNDLE

ORIGIN = 'http://gateway-bindings.internal'


class CloudflareBindingBridge:
    def __init__(self, manifests, descriptors, *, opener=None):
        self.manifests = {pin: canonical(manifest) for pin, manifest in manifests.items()}
        self.descriptors = dict(descriptors)
        self.opener = opener or urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def read(self, path, maximum):
        request = urllib.request.Request(ORIGIN + path,
            headers={'Accept': 'application/octet-stream'}, method='GET')
        try:
            with self.opener.open(request, timeout=15) as response:
                if response.status != 200 or response.headers.get('Content-Type') != 'application/octet-stream':
                    raise Refused('binding response unavailable')
                if response.headers.get('Content-Encoding'):
                    raise Refused('encoded binding response refused')
                length = response.headers.get('Content-Length')
                if length is not None and (not length.isascii() or not length.isdecimal() or int(length) > maximum):
                    raise Refused('binding response limit')
                data = response.read(maximum + 1)
                if len(data) > maximum or length is not None and len(data) != int(length):
                    raise Refused('binding response limit')
                return data
        except (OSError, urllib.error.URLError, ValueError):
            raise Refused('binding response unavailable') from None

    def resolve(self, pin):
        if not isinstance(pin, str) or not OID.fullmatch(pin) or pin not in self.manifests:
            raise Refused('unpublished catalog pin')
        data = self.read('/catalog/' + pin, MAX_MANIFEST)
        manifest = validate(data)
        if data != self.manifests[pin]:
            raise Refused('catalog differs from approved immutable view')
        return manifest

    def read_bundle(self, source):
        if not isinstance(source, str) or not NAME.fullmatch(source) or source not in self.descriptors:
            raise Refused('unapproved native source')
        return self.read('/native/' + source, MAX_BUNDLE)
