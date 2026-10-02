#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run Claude with invocation-only attribution hooks; stream stdout unchanged."""
import argparse
import json
import os
from pathlib import Path
import shlex
import signal
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading

from collector import Collector, MAX_BYTES, Sink


def receive(stream):
    raw = stream.readline(MAX_BYTES + 1)
    if len(raw) > MAX_BYTES or not raw.endswith(b'\n'):
        raise ValueError('oversize/incomplete event')
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError('invalid event')
    return value


def hook(address):
    raw = sys.stdin.buffer.read(MAX_BYTES + 1)
    if len(raw) > MAX_BYTES:
        raise ValueError('oversize hook')
    event = json.loads(raw)
    if not isinstance(event, dict):
        raise ValueError('invalid hook')
    # Only paths and native identity cross the private local socket.
    event = {k: event[k] for k in ('hook_event_name', 'cwd', 'session_id', 'agent_id',
                                  'tool_use_id', 'tool_name', 'tool_input') if k in event}
    args = event.get('tool_input')
    event['tool_input'] = {k: args[k] for k in ('file_path', 'notebook_path') if k in args} if isinstance(args, dict) else {}
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(15)
        client.connect(address)
        client.sendall(json.dumps(event).encode() + b'\n')
        if client.recv(16) != b'ok\n':
            raise RuntimeError('collector rejected hook')


def run(args):
    root = Path(args.repo).resolve(strict=True)
    result = subprocess.run([args.claude, '--version'], capture_output=True, timeout=10, text=True)
    version = result.stdout.split()[0] if result.returncode == 0 and result.stdout else ''
    collector = Collector('claude-code', version, root, Sink(args.heddle, root))
    # Collector owns these options so user settings cannot replace its hooks.
    reserved = ('--settings', '--setting-sources', '--output-format', '--verbose',
                '--no-session-persistence', '--include-hook-events', '--resume', '--continue')
    command = args.args[1:] if args.args[:1] == ['--'] else args.args
    if any(a.split('=')[0] in reserved or a in ('-r', '-c') for a in command):
        raise ValueError('collector-owned output/settings/session options cannot be overridden')
    failures = threading.Event()
    with tempfile.TemporaryDirectory(prefix='heddle-claude-') as temporary:
        directory = Path(temporary)
        address = str(directory / 'events.sock')

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                self.request.settimeout(5)
                try:
                    collector.hook(receive(self.rfile))
                    self.wfile.write(b'ok\n')
                except Exception:
                    failures.set()
                    self.wfile.write(b'error\n')

        server = socketserver.UnixStreamServer(address, Handler)
        os.chmod(address, 0o600)
        worker = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.1}, daemon=True)
        worker.start()
        invocation = shlex.join([sys.executable, str(Path(__file__).resolve()), 'hook', address])
        settings = directory / 'settings.json'
        settings.write_text(json.dumps({'hooks': {name: [{'matcher': '', 'hooks': [
            {'type': 'command', 'command': invocation, 'timeout': 15}]}]
            for name in ('PreToolUse', 'PostToolUse', 'PostToolUseFailure')}}))
        os.chmod(settings, 0o600)
        proc = None
        expired = threading.Event()

        def terminate():
            expired.set()
            if proc is not None:
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                    proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                except ProcessLookupError:
                    pass

        timer = threading.Timer(args.timeout, terminate)
        try:
            proc = subprocess.Popen([args.claude, '-p', args.prompt, '--setting-sources', '', '--settings', str(settings),
                                     '--output-format', 'stream-json', '--verbose', '--no-session-persistence',
                                     *command], cwd=root, env=dict(os.environ, PWD=str(root)),
                                    stdout=subprocess.PIPE, start_new_session=True)
            timer.start()
            oversized = False
            while True:
                chunk = proc.stdout.readline(MAX_BYTES + 1)
                if not chunk:
                    break
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
                if len(chunk) > MAX_BYTES:
                    oversized = True
                    continue
                if oversized:
                    oversized = not chunk.endswith(b'\n')
                    continue
                try:
                    collector.claude_message(json.loads(chunk))
                except (ValueError, TypeError, RecursionError):
                    continue
            code = proc.wait(timeout=3)
            return 124 if expired.is_set() else (1 if failures.is_set() else code)
        finally:
            timer.cancel()
            if proc is not None and proc.poll() is None:
                terminate()
                proc.wait(timeout=3)
            server.shutdown()
            server.server_close()
            worker.join(timeout=2)


def main():
    if len(sys.argv) == 3 and sys.argv[1] == 'hook':
        hook(sys.argv[2])
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--prompt', required=True, help='Task for this fresh Claude invocation')
    parser.add_argument('--heddle', required=True)
    parser.add_argument('--claude', default='claude')
    parser.add_argument('--timeout', type=int, default=120, choices=range(1, 601), metavar='1..600')
    parser.add_argument('args', nargs=argparse.REMAINDER)
    return run(parser.parse_args())


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception:
        # Never echo source payloads, arguments or provider errors.
        print('attribution adapter failed; no source payload logged', file=sys.stderr)
        sys.exit(1)
