# SPDX-License-Identifier: Apache-2.0
"""Small opt-in bearer authority; consumes pre-provisioned policy, never mints keys."""
import hashlib
import hmac
import json
import math
import re
import time
from pathlib import Path
from .core import Refused

MAX_POLICY = 64 * 1024


def read_json(path):
    """Bounded, unambiguous local configuration; never include its data in errors."""
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError('duplicate field')
            result[key] = value
        return result
    try:
        with Path(path).open('rb') as source:
            data = source.read(MAX_POLICY + 1)
        if len(data) > MAX_POLICY:
            raise ValueError('configuration limit')
        document = json.loads(data, object_pairs_hook=pairs)
        if not isinstance(document, dict):
            raise ValueError('configuration object required')
        return document
    except (OSError, ValueError, UnicodeError):
        raise Refused('authority configuration unavailable') from None


def credential_digest(credential):
    if not isinstance(credential, str) or not re.fullmatch(r'Bearer [A-Za-z0-9_-]{32,256}', credential):
        raise Refused('invalid credential')
    return hashlib.sha256(credential[7:].encode('ascii')).hexdigest()


def current_grant(document, category, credential, now):
    digest = credential_digest(credential)
    grants = document.get(category)
    if set(document) != {category} or not isinstance(grants, list) or len(grants) > 1024:
        raise Refused('invalid authority policy')
    match = None
    for grant in grants:
        if not isinstance(grant, dict):
            raise Refused('invalid authority grant')
        stored = grant.get('sha256', '')
        if not isinstance(stored, str) or not re.fullmatch('[0-9a-f]{64}', stored):
            raise Refused('invalid authority digest')
        if hmac.compare_digest(digest, stored):
            if match is not None:
                raise Refused('ambiguous authority grant')
            match = grant
    if match is None:
        raise Refused('denied')
    expires = match.get('expires_at')
    if (type(expires) not in (int, float) or not 0 <= expires <= 9007199254740991 or
        (type(expires) is float and not math.isfinite(expires)) or not now < expires):
        raise Refused('expired credential')
    return match

class BearerAuthorization:
    def __init__(self, policy, clock=time.time):
        self.policy, self.clock = Path(policy), clock

    def authorize(self, credential, manifest):
        # Both GET and POST reload authority, so revocation/epoch changes take effect.
        document = read_json(self.policy)
        scope = [manifest[key] for key in ('repository', 'source', 'thread', 'state', 'policy_epoch')]
        reader = current_grant(document, 'readers', credential, self.clock())
        views = reader.get('views')
        if set(reader) != {'sha256', 'expires_at', 'views'} or not isinstance(views, list) or len(views) > 1024:
            raise Refused('invalid reader scope')
        if any(not isinstance(view, list) or len(view) != 5 or type(view[4]) is not int for view in views):
            raise Refused('invalid reader scope')
        if scope not in views:
            raise Refused('denied or stale policy epoch')


class ServiceAuthorization:
    """Pre-provisioned hop identity, scoped to immutable catalog pins, never a reader grant."""
    def __init__(self, policy, clock=time.time):
        self.policy, self.clock = Path(policy), clock

    def authorize(self, credential, pin):
        service = current_grant(read_json(self.policy), 'services', credential, self.clock())
        pins = service.get('pins')
        if (set(service) != {'sha256', 'expires_at', 'pins'} or not isinstance(pins, list) or
            len(pins) > 1024 or any(not isinstance(p, str) or not re.fullmatch('[0-9a-f]{40}', p) for p in pins)):
            raise Refused('invalid service scope')
        if pin not in pins:
            raise Refused('denied service pin')
