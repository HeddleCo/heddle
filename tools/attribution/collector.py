# SPDX-License-Identifier: Apache-2.0
"""Bounded exact-ID adapters. No transcript discovery, credentials, or exports."""
import json
from pathlib import Path
import re
import subprocess
import threading

MAX_BYTES = 1024 * 1024
MAX_OPERATIONS = 64


def identifier(value):
    """Admit identity atoms only; canonical Rust admission remains authoritative."""
    if not isinstance(value, str) or not 0 < len(value.encode()) <= 256:
        return None
    if not re.fullmatch(r"[\w./:@+\-]+", value, re.ASCII):
        return None
    if '://' in value or value.lower().startswith(('sk-', 'ghp_', 'bearer')):
        return None
    return value


def claim(value, basis='observed', source='harness_hook'):
    value = identifier(value)
    return {'value': value, 'basis': basis, 'source': source} if value else None


def evidence(harness, version, scope):
    return {'format_version': 1, 'harness': claim(harness),
            'harness_version': claim(version, 'observed', 'process'), 'harness_version_scope': 'current_invocation',
            'selected': {}, 'response': {}, 'scope': scope}


def bounded_paths(root, tool, args, harness):
    """Known structured file tools only. Shell and Hermes V4A stay opaque."""
    if not isinstance(args, dict):
        return []
    path = None
    if harness == 'claude-code' and tool in ('Write', 'Edit', 'MultiEdit'):
        path = args.get('file_path')
    elif harness == 'claude-code' and tool == 'NotebookEdit':
        path = args.get('notebook_path')
    elif harness == 'hermes' and (tool == 'write_file' or
                                 (tool == 'patch' and args.get('mode', 'replace') == 'replace')):
        path = args.get('path')
    if not isinstance(path, str) or not 0 < len(path.encode()) <= 1024 or '\0' in path:
        return []
    # Reject outside/symlink escapes; Rust validates again at every snapshot.
    if harness == 'hermes' and not Path(path).is_absolute():
        return []  # Hermes task working directories can differ from process cwd.
    candidate = (root / path).resolve()
    if not candidate.is_relative_to(root):
        return []
    return [str(candidate)]


class Sink:
    """Normalize through the same typed Rust boundary as other collectors."""
    def __init__(self, binary, root):
        self.binary = str(Path(binary).resolve(strict=True))
        self.root = Path(root).resolve(strict=True)

    def __call__(self, observation):
        raw = json.dumps(observation).encode()
        if len(raw) > 64 * 1024:
            raise ValueError('normalized observation exceeds bound')
        result = subprocess.run([self.binary, '--repo', str(self.root), 'integration', 'collect'],
                                input=raw, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=10)
        if result.returncode:
            raise RuntimeError('attribution collection failed')


class Collector:
    """Invocation-local joins; saturation drops enrichment without evicting IDs.

    Two differing observations are retained, then the key stays poisoned. The
    Rust journal preserves conflict/unresolved evidence; arrival order cannot
    make a conflict valid again. Published states are never rewritten.
    """
    def __init__(self, harness, version, root, emit, local_files=False):
        supported = {'claude-code': '2.1.287', 'hermes': '0.21.0'}
        if supported.get(harness) != version:
            raise ValueError('unsupported harness version')
        self.harness, self.version = harness, version
        self.local_files = local_files
        self.root, self.emit = Path(root).resolve(), emit
        self.models, self.operations = {}, {}
        self.lock = threading.RLock()

    def key(self, event):
        fields = ('session_id', 'tool_use_id') if self.harness == 'claude-code' else (
            'session_id', 'turn_id', 'api_request_id', 'tool_call_id')
        values = tuple(identifier(event.get(f)) for f in fields)
        return values if all(values) else None

    def observe(self, key, metadata):
        with self.lock:
            if key not in self.models:
                if len(self.models) >= MAX_OPERATIONS:
                    return
                self.models[key] = []
            values = self.models[key]
            if metadata in values or len(values) >= 2:
                return
            values.append(metadata)
            if key in self.operations:
                self.enrich(self.operations[key], metadata)

    def enrich(self, base, metadata):
        joined = json.loads(json.dumps(base))
        joined['selected'] = metadata.get('selected', {})
        joined['response'] = metadata.get('response', {})
        joined['scope'].update(metadata.get('scope', {}))
        self.emit({'method': 'event_stream' if self.harness == 'claude-code' else 'hook',
                   'identity': joined, 'phase': 'observe', 'paths': []})

    def claude_message(self, event):
        if not isinstance(event, dict) or event.get('type') != 'assistant':
            return
        message = event.get('message')
        if not isinstance(message, dict):
            return
        model, response = identifier(message.get('model')), identifier(message.get('id'))
        session = identifier(event.get('session_id'))
        if not all((model, response, session)):
            return
        content = message.get('content')
        if not isinstance(content, list) or len(content) > MAX_OPERATIONS:
            return
        for block in content:
            if not isinstance(block, dict) or block.get('type') != 'tool_use':
                continue
            tool = identifier(block.get('id'))
            if tool:
                scope = {'response_id': response}
                request = identifier(event.get('request_id'))
                if request:
                    scope['request_id'] = request
                self.observe((session, tool), {'response': {'model': claim(model, 'response_reported', 'response')},
                                              'scope': scope})

    def hermes_response(self, event):
        # Installed 0.21.0 post_api_request supplies a sanitized response dict.
        response = event.get('response')
        if not isinstance(response, dict):
            return
        assistant = response.get('assistant_message')
        calls = assistant.get('tool_calls') if isinstance(assistant, dict) else None
        if not isinstance(calls, list) or len(calls) > MAX_OPERATIONS:
            return
        selected = {k: claim(event.get(k), 'request_reported', 'request') for k in ('model', 'provider')}
        model = claim(event.get('response_model'), 'response_reported', 'response')
        for call in calls:
            if not isinstance(call, dict):
                continue
            key = self.key({**event, 'tool_call_id': call.get('id')})
            if key:
                self.observe(key, {'selected': selected, 'response': {'model': model}})

    def hook(self, event):
        """Called synchronously before/after mutation; no tool bodies retained."""
        if not isinstance(event, dict):
            raise ValueError('invalid hook')
        with self.lock:
            name = event.get('hook_event_name')
            phases = {'PreToolUse': 'before', 'PostToolUse': 'after', 'PostToolUseFailure': 'failed'}
            if self.harness == 'hermes':
                name = event.get('event')
                phases = {'pre_tool_call': 'before', 'post_tool_call': 'after'}
                if name == 'post_api_request':
                    self.hermes_response(event)
                    return
            if name not in phases:
                return
            cwd = event.get('cwd')
            if cwd and (not isinstance(cwd, str) or Path(cwd).resolve() != self.root):
                raise ValueError('hook workspace mismatch')
            key = self.key(event)
            scope = {'harness_session_id': identifier(event.get('session_id')),
                     'actor_id': identifier(event.get('agent_id')) or identifier(event.get('session_id')),
                     'tool_call_id': identifier(event.get('tool_use_id' if self.harness == 'claude-code' else 'tool_call_id'))}
            if self.harness == 'hermes':
                scope.update(turn_id=identifier(event.get('turn_id')), request_id=identifier(event.get('api_request_id')))
            elif identifier(event.get('agent_id')):
                scope['parent_harness_session_id'] = identifier(event.get('session_id'))
            base = evidence(self.harness, self.version, scope)
            phase = phases[name]
            if self.harness == 'hermes' and phase == 'after' and event.get('status') != 'ok':
                phase = 'failed'
            paths = bounded_paths(self.root, event.get('tool_name'), event.get('tool_input', event.get('args')), self.harness)
            if self.harness == 'hermes' and not self.local_files:
                paths = []
            self.emit({'method': 'hook', 'identity': base, 'phase': phase, 'paths': paths})
            if key:
                if key not in self.operations and len(self.operations) < MAX_OPERATIONS:
                    self.operations[key] = base
                # Use the original actor scope, not a later mutable identity.
                base = self.operations.get(key)
                if base:
                    for metadata in self.models.get(key, []):
                        self.enrich(base, metadata)
