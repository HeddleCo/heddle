# SPDX-License-Identifier: Apache-2.0
"""Loopback reference server. Production identity/TLS/front-door wiring is unimplemented."""
import argparse
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import re
import subprocess
import tempfile
from urllib.parse import urlsplit
from .core import GIT, Refused, LocalCatalog, FixtureAuthorization, NativeSource, git_env, bounded_process

MAX_REQUEST = 1024 * 1024
MAX_RESPONSE = 96 * 1024 * 1024
ROUTE = re.compile(r'/views/([0-9a-f]{40})\.git/(info/refs|git-upload-pack)')


def server(catalog, source, authorization, port=0, *, bearer_mode=False, service_authorization=None):
    if service_authorization is not None and not bearer_mode:
        raise ValueError('service authorization requires reader bearer authorization')
    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(10)

        def log_message(self, *_):
            pass  # no credentials or source paths in logs

        def reply(self, status, body=b'', headers=()):
            self.send_response(status)
            for key, value in headers:
                self.send_header(key, value)
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.serve_git()

        def do_POST(self):
            self.serve_git()

        def serve_git(self):
            try:
                # Fixture selectors are intentionally unusable at a normal edge origin.
                # This blocks accidental direct/Worker/reverse-proxy wiring and DNS
                # rebinding; a malicious operator could still deliberately tunnel it.
                host = self.headers.get_all('Host', [])
                allowed_hosts = {f'127.0.0.1:{self.server.server_port}', f'localhost:{self.server.server_port}'}
                if self.client_address[0] != '127.0.0.1' or len(host) != 1 or host[0] not in allowed_hosts:
                    return self.reply(403)
                if any(self.headers.get(name) is not None for name in
                       ('Origin', 'Forwarded', 'X-Forwarded-For', 'X-Forwarded-Host', 'X-Real-IP')):
                    return self.reply(403)
                if bearer_mode:
                    if len(self.headers.get_all('Authorization', [])) != 1 or self.headers.get('X-Demo-Reader') is not None:
                        return self.reply(403)
                    credential = self.headers['Authorization']
                else:
                    if self.headers.get('Authorization') is not None or len(self.headers.get_all('X-Demo-Reader', [])) != 1:
                        return self.reply(403)
                    credential = self.headers['X-Demo-Reader']
                url = urlsplit(self.path)
                match = ROUTE.fullmatch(url.path)
                if not match or '%' in self.path or url.fragment or url.scheme or url.netloc:
                    return self.reply(404)
                pin, endpoint = match.groups()
                if (self.command, endpoint, url.query) not in (
                    ('GET', 'info/refs', 'service=git-upload-pack'), ('POST', 'git-upload-pack', '')
                ):
                    return self.reply(405)
                service_headers = self.headers.get_all('X-Gateway-Service-Authorization', [])
                if service_authorization is not None:
                    if len(service_headers) != 1:
                        return self.reply(403)
                    # The hop identity is checked before any catalog/source reads.
                    # It never substitutes for the independent reader view grant.
                    service_authorization.authorize(service_headers[0], pin)
                elif service_headers:
                    return self.reply(403)
                if self.headers.get('Transfer-Encoding') or self.headers.get('Content-Encoding'):
                    return self.reply(400)
                lengths = self.headers.get_all('Content-Length', [])
                if len(lengths) > 1:
                    return self.reply(400)
                if lengths and (not lengths[0].isascii() or not lengths[0].isdecimal()):
                    return self.reply(400)
                length = int(lengths[0]) if lengths else 0
                if not 0 <= length <= MAX_REQUEST:
                    return self.reply(413)
                if self.command == 'GET' and length:
                    return self.reply(400)
                if self.command == 'POST' and self.headers.get('Content-Type') != 'application/x-git-upload-pack-request':
                    return self.reply(415)
                manifest = catalog.resolve(pin)
                authorization.authorize(credential, manifest)
                body = self.rfile.read(length)
                if len(body) != length:
                    return self.reply(400)
                with tempfile.TemporaryDirectory(prefix='heddle-gateway-') as temp:
                    destination = Path(temp) / 'view.git'
                    source.materialize(manifest, destination)
                    env = git_env()
                    env.update({'GIT_PROJECT_ROOT': temp, 'GIT_HTTP_EXPORT_ALL': '1',
                                'PATH_INFO': '/view.git/' + endpoint, 'QUERY_STRING': url.query,
                                'REQUEST_METHOD': self.command, 'CONTENT_TYPE': self.headers.get('Content-Type', ''),
                                'CONTENT_LENGTH': str(length), 'REMOTE_ADDR': '127.0.0.1'})
                    protocol = self.headers.get('Git-Protocol', '')
                    if protocol in ('version=1', 'version=2'):
                        env['GIT_PROTOCOL'] = protocol
                    # File-backed output avoids accumulating a maliciously large response in RAM.
                    with tempfile.TemporaryFile() as output:
                        p = bounded_process([GIT, 'http-backend'], input=body, stdout=output,
                                           stderr=subprocess.DEVNULL, env=env, timeout=30)
                        size = output.tell()
                        if p.returncode or size > MAX_RESPONSE:
                            raise Refused('backend failed or response limit')
                        output.seek(0)
                        raw = output.read()
                    head, content = raw.split(b'\r\n\r\n', 1)
                    headers = []
                    status = 200
                    for line in head.decode('ascii').split('\r\n'):
                        key, value = line.split(':', 1)
                        if key.lower() == 'status':
                            status = int(value.strip().split()[0])
                        elif key.lower() == 'content-type':
                            headers.append((key, value.strip()))
                    self.reply(status, content, headers)
            except Refused:
                self.reply(403, b'View unavailable or access denied\n')
            except (ValueError, OSError, subprocess.SubprocessError):
                self.reply(503, b'View unavailable\n')

    # Deliberately one active request with a bounded listen queue for this small MVP.
    return HTTPServer(('127.0.0.1', port), Handler)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('config', type=Path)
    parser.add_argument('--port', type=int, default=8042)
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    app = server(LocalCatalog(config['catalog']), NativeSource(config['binary'], config['sources']),
                 FixtureAuthorization(config['policy']), args.port)
    print(f'Loopback fixture gateway on http://127.0.0.1:{app.server_port}', flush=True)
    app.serve_forever()

if __name__ == '__main__':
    main()
