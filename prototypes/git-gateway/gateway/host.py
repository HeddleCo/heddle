# SPDX-License-Identifier: Apache-2.0
"""Authenticated Linux host entrypoint. Loopback-only; TLS/edge deployment is separate."""
import argparse
import os
from pathlib import Path
import re
import resource
import sys
from .artifacts import ArtifactsCatalog
from .auth import BearerAuthorization, ServiceAuthorization, read_json
from .bundle_transport import HTTPSBundleReader
from .core import LocalCatalog, NAME, Refused
from .http import server
from .native_bundle import MAX_BUNDLE, NativeBundleSource


def credential_file(path):
    try:
        with Path(path).open('r', encoding='ascii') as file:
            raw = file.read(258)
        if len(raw) > 257:
            raise ValueError('credential file limit')
        value = raw.removesuffix('\n')
        if not re.fullmatch(r'[A-Za-z0-9_-]{32,256}', value):
            raise ValueError('invalid credential')
        return value
    except (OSError, ValueError, UnicodeError):
        raise Refused('pre-provisioned credential file unavailable') from None


def exact(document, required, optional=()):
    if not isinstance(document, dict) or not set(required) <= set(document) <= set(required) | set(optional):
        raise Refused('invalid host configuration')


def configured_server(config):
    exact(config, {'schema', 'binary', 'reader_policy', 'service_policy', 'catalog', 'native'}, {'port'})
    if type(config['schema']) is not int or config['schema'] != 1:
        raise Refused('invalid host schema')
    port = config.get('port', 8042)
    if type(port) is not int or not 0 <= port <= 65535:
        raise Refused('invalid host port')
    binary = Path(config['binary'])
    if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise Refused('native exporter unavailable')
    # No fixture selector or generated authority can be selected in this entrypoint.
    for key, category in [('reader_policy', 'readers'), ('service_policy', 'services')]:
        document = read_json(config[key])
        if set(document) != {category} or not isinstance(document[category], list):
            raise Refused('authority policy required')
    catalog_config = config['catalog']
    if not isinstance(catalog_config, dict):
        raise Refused('invalid catalog configuration')
    if catalog_config.get('kind') == 'local':
        exact(catalog_config, {'kind', 'path'})
        path = Path(catalog_config['path'])
        if not path.is_dir() or not (path / 'HEAD').is_file():
            raise Refused('existing read-only catalog required')
        catalog = LocalCatalog(path)  # Existing path only; never initialize/publish here.
    elif catalog_config.get('kind') == 'artifacts':
        exact(catalog_config, {'kind', 'account', 'namespace', 'repository', 'published_pins', 'credential_file'})
        catalog = ArtifactsCatalog(catalog_config['account'], catalog_config['namespace'],
            catalog_config['repository'], catalog_config['published_pins'], credential_file(catalog_config['credential_file']))
    else:
        raise Refused('unsupported catalog configuration')
    native = config['native']
    if not isinstance(native, dict):
        raise Refused('invalid native configuration')
    descriptors = native.get('descriptors')
    if (not isinstance(descriptors, dict) or not 1 <= len(descriptors) <= 1024 or
        any(not isinstance(k, str) or not NAME.fullmatch(k) or not isinstance(v, str) or
            not re.fullmatch('[0-9a-f]{64}', v) for k, v in descriptors.items())):
        raise Refused('invalid native descriptors')
    if native.get('kind') == 'https':
        exact(native, {'kind', 'descriptors', 'origin', 'credential_file'}, {'authorized_threads'})
        reader = HTTPSBundleReader(native['origin'], credential_file(native['credential_file']))
    elif native.get('kind') == 'local-bundles':
        exact(native, {'kind', 'descriptors', 'paths'}, {'authorized_threads'})
        paths = native['paths']
        if not isinstance(paths, dict) or set(paths) != set(descriptors):
            raise Refused('invalid local bundle configuration')
        resolved = {}
        for name, value in paths.items():
            path = Path(value)
            if not path.is_absolute() or path.is_symlink() or not path.is_file():
                raise Refused('local bundle unavailable')
            resolved[name] = path
        def reader(name):
            try:
                with resolved[name].open('rb') as source:
                    return source.read(MAX_BUNDLE + 1)
            except (OSError, KeyError):
                raise Refused('local bundle unavailable') from None
    else:
        raise Refused('unsupported native configuration')
    return server(catalog, NativeBundleSource(binary, descriptors, reader, native.get('authorized_threads')),
        BearerAuthorization(config['reader_policy']), port, bearer_mode=True,
        service_authorization=ServiceAuthorization(config['service_policy']))


def runtime_limits():
    if not sys.platform.startswith('linux') or os.geteuid() == 0:
        raise Refused('authenticated host requires non-root Linux')
    os.umask(0o077)
    # Supplement, not replace, the container's cgroup memory and tmpfs disk limits.
    for kind, requested in [(resource.RLIMIT_CORE, 0), (resource.RLIMIT_AS, 1536 * 1024 * 1024),
                            (resource.RLIMIT_NOFILE, 256), (resource.RLIMIT_FSIZE, 96 * 1024 * 1024)]:
        _, hard = resource.getrlimit(kind)
        limit = requested if hard == resource.RLIM_INFINITY else min(requested, hard)
        resource.setrlimit(kind, (limit, limit))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', type=Path)
    args = parser.parse_args()
    try:
        runtime_limits()
        app = configured_server(read_json(args.config))
        print(f'Authenticated internal gateway on 127.0.0.1:{app.server_port}', flush=True)
        app.serve_forever()
    except (Refused, ValueError, TypeError, KeyError, OSError):
        raise SystemExit('Authenticated host unavailable; check configuration and runtime limits') from None


if __name__ == '__main__':
    main()
