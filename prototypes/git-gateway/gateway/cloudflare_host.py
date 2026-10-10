# SPDX-License-Identifier: Apache-2.0
"""Bounded synthetic Cloudflare demo bootstrap; deployment/identity are external."""
import http.client
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import re
import tempfile
import threading
import time
from urllib.parse import urlsplit
from .auth import BearerAuthorization, ServiceAuthorization, read_json
from .cloudflare_bridge import CloudflareBindingBridge
from .core import OID, Refused, canonical, thread_grants
from .host import exact, runtime_limits
from .http import MAX_REQUEST, MAX_RESPONSE, ROUTE, server
from .native_bundle import NativeBundleSource


def validate_config(config, *, now=None):
    now = time.time() if now is None else now
    exact(config, {'schema', 'expires_at', 'published_pins', 'reader_sha256', 'service_sha256',
        'views', 'descriptors', 'authorized_threads'})
    expires = config['expires_at']
    if config['schema'] != 1 or type(config['schema']) is not int or type(expires) is not int or not now < expires <= now + 900:
        raise Refused('expired or invalid bounded demo policy')
    for key in ('reader_sha256', 'service_sha256'):
        if not isinstance(config[key], str) or not re.fullmatch('[0-9a-f]{64}', config[key]):
            raise Refused('credential digest required')
    if config['reader_sha256'] == config['service_sha256']:
        raise Refused('independent reader and service identities required')
    pins, views = config['published_pins'], config['views']
    if (not isinstance(pins, list) or not 1 <= len(pins) <= 2 or
        any(not isinstance(pin, str) or not OID.fullmatch(pin) for pin in pins) or
        pins != sorted(set(pins)) or not isinstance(views, list) or len(views) != len(pins)):
        raise Refused('exact bounded published views required')
    for manifest in views:
        canonical(manifest)
    sources = {manifest['source'] for manifest in views}
    descriptors = config['descriptors']
    if (len(sources) != 1 or not isinstance(descriptors, dict) or set(descriptors) != sources or
        any(not isinstance(digest, str) or not re.fullmatch('[0-9a-f]{64}', digest) for digest in descriptors.values())):
        raise Refused('one approved digest-pinned native source required')
    grants = thread_grants(config['authorized_threads'])
    if set(grants) != sources or any(manifest['thread'] not in grants[manifest['source']] for manifest in views):
        raise Refused('explicit governing and dependency Thread grants required')
    return config


def native_server(config, binary, directory, *, port=8042, bridge=None):
    """Build the existing loopback host; raw credentials never appear in config."""
    validate_config(config)
    directory = Path(directory)
    readers = directory / 'readers.json'
    services = directory / 'services.json'
    readers.write_text(json.dumps({'readers': [{'sha256': config['reader_sha256'], 'expires_at': config['expires_at'],
        'views': [[m[k] for k in ('repository', 'source', 'thread', 'state', 'policy_epoch')] for m in config['views']]}]}))
    services.write_text(json.dumps({'services': [{'sha256': config['service_sha256'], 'expires_at': config['expires_at'],
        'pins': config['published_pins']}]}))
    bridge = bridge or CloudflareBindingBridge(dict(zip(config['published_pins'], config['views'])), config['descriptors'])
    authority = ServiceAuthorization(services)
    app = server(bridge, NativeBundleSource(binary, config['descriptors'], bridge.read_bundle,
        config['authorized_threads']), BearerAuthorization(readers), port,
        bearer_mode=True, service_authorization=authority)
    return app, authority


def ingress(native_port, authority, *, bind='0.0.0.0', port=8080):
    """One request at a time, no general proxy: only fixed native loopback routes."""
    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup(); self.connection.settimeout(10)

        def log_message(self, *_): pass

        def reply(self, status, body=b'', content_type=None):
            self.send_response(status)
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Content-Length', str(len(body)))
            if content_type: self.send_header('Content-Type', content_type)
            self.end_headers(); self.wfile.write(body)

        def do_GET(self): self.forward()
        def do_POST(self): self.forward()

        def forward(self):
            connection = None
            try:
                hosts = self.headers.get_all('Host', [])
                if len(hosts) != 1 or hosts[0] not in ('native-container.invalid', 'native-container.invalid:8080'):
                    return self.reply(403)
                url = urlsplit(self.path)
                match = ROUTE.fullmatch(url.path)
                if not match or '%' in self.path or url.scheme or url.netloc or url.fragment:
                    return self.reply(404)
                pin, endpoint = match.groups()
                if (self.command, endpoint, url.query) not in [('GET', 'info/refs', 'service=git-upload-pack'), ('POST', 'git-upload-pack', '')]:
                    return self.reply(405)
                for name in ('Origin', 'Forwarded', 'X-Forwarded-For', 'X-Forwarded-Host', 'X-Real-IP', 'X-Demo-Reader'):
                    if self.headers.get(name) is not None: return self.reply(403)
                if any(len(self.headers.get_all(name, [])) != 1 for name in ('Authorization', 'X-Gateway-Service-Authorization')):
                    return self.reply(403)
                authority.authorize(self.headers['X-Gateway-Service-Authorization'], pin)
                if self.headers.get('Transfer-Encoding') or self.headers.get('Content-Encoding'):
                    return self.reply(400)
                values = self.headers.get_all('Content-Length', [])
                if len(values) > 1 or values and (not values[0].isascii() or not values[0].isdecimal()):
                    return self.reply(400)
                size = int(values[0]) if values else 0
                if size > MAX_REQUEST: return self.reply(413)
                if self.command == 'GET' and size: return self.reply(400)
                if self.command == 'POST' and self.headers.get('Content-Type') != 'application/x-git-upload-pack-request':
                    return self.reply(415)
                body = self.rfile.read(size)
                if len(body) != size: return self.reply(400)
                headers = {name: self.headers[name] for name in ('Authorization', 'X-Gateway-Service-Authorization', 'Content-Type', 'Git-Protocol') if self.headers.get(name) is not None}
                headers['Content-Length'] = str(len(body))
                # http.client supplies the correct local Host. Incoming Host, proxy
                # credentials and arbitrary CGI-style fields never reach native Git.
                connection = http.client.HTTPConnection('127.0.0.1', native_port, timeout=30)
                connection.request(self.command, url.path + ('?' + url.query if url.query else ''), body=body, headers=headers)
                response = connection.getresponse()
                length = response.getheader('Content-Length')
                if length is None or not length.isdecimal() or int(length) > MAX_RESPONSE:
                    raise Refused('native response limit')
                data = response.read(MAX_RESPONSE + 1)
                if len(data) != int(length): raise Refused('native response length')
                self.reply(response.status, data, response.getheader('Content-Type'))
            except Refused:
                self.reply(403)
            except (OSError, ValueError, http.client.HTTPException):
                self.reply(503)
            finally:
                if connection is not None: connection.close()
    return HTTPServer((bind, port), Handler)


def main():
    runtime_limits()
    raw = os.environ.pop('GATEWAY_DEMO_CONFIG_JSON', '')
    if not raw or len(raw.encode()) > 65536:
        raise SystemExit('Approved bounded demo configuration required')
    with tempfile.TemporaryDirectory(prefix='gateway-cloudflare-') as directory:
        path = Path(directory) / 'config.json'; path.write_text(raw)
        try:
            config = validate_config(read_json(path))
            native, authority = native_server(config, '/usr/local/bin/gateway_native', directory)
            front = ingress(native.server_port, authority)
        except (Refused, OSError, ValueError, TypeError, KeyError):
            raise SystemExit('Cloudflare demo host unavailable; check approved configuration') from None
        thread = threading.Thread(target=native.serve_forever, daemon=True); thread.start()
        print('Bounded authenticated synthetic demo ingress on port 8080', flush=True)
        try:
            front.serve_forever()
        finally:
            front.server_close(); native.shutdown(); thread.join(); native.server_close()


if __name__ == '__main__':
    main()
