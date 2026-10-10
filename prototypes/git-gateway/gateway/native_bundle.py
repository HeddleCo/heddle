# SPDX-License-Identifier: Apache-2.0
"""Bounded synthetic native bundles. No Git cache, credentials or hosted writes."""
import hashlib
import io
from pathlib import Path, PurePosixPath
import stat
import tarfile
import tempfile
from .core import NativeSource, Refused, thread_grants

MAX_BUNDLE = 8 * 1024 * 1024
OMIT = {'identity.toml', 'git-projection', 'state', 'materialized-roots'}


def pack_fixture(repository):
    root = Path(repository) / '.heddle'
    if root.is_symlink() or not root.is_dir():
        raise Refused('invalid native fixture')
    output = io.BytesIO()
    count = total = 0
    with tarfile.open(fileobj=output, mode='w', format=tarfile.PAX_FORMAT) as archive:
        for path in sorted(root.rglob('*')):
            relative = path.relative_to(root)
            if any(part in OMIT for part in relative.parts):
                continue
            mode = path.lstat().st_mode
            if stat.S_ISDIR(mode):
                continue
            if not stat.S_ISREG(mode):
                raise Refused('unsupported fixture entry')
            count += 1
            total += path.stat().st_size
            if count > 10000 or total > MAX_BUNDLE:
                raise Refused('bundle limit')
            data = path.read_bytes()
            info = tarfile.TarInfo('.heddle/' + relative.as_posix())
            info.size = len(data)
            info.mode = 0o600
            archive.addfile(info, io.BytesIO(data))
    data = output.getvalue()
    if len(data) > MAX_BUNDLE:
        raise Refused('bundle limit')
    return data


def unpack_fixture(data, destination):
    if not isinstance(data, bytes) or len(data) > MAX_BUNDLE:
        raise Refused('bundle limit')
    seen = set()
    total = 0
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode='r:') as archive:
            for member in archive:
                path = PurePosixPath(member.name)
                if (not member.isfile() or member.name != path.as_posix() or
                    path.is_absolute() or '..' in path.parts or len(path.parts) < 2 or
                    path.parts[0] != '.heddle' or '\\' in member.name or
                    any(part in OMIT for part in path.parts) or
                    member.name.casefold() in seen):
                    raise Refused('unsafe bundle entry')
                seen.add(member.name.casefold())
                total += member.size
                if len(seen) > 10000 or total > MAX_BUNDLE or member.size < 0:
                    raise Refused('bundle limit')
                target = Path(destination).joinpath(*path.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open('xb') as out:
                    out.write(archive.extractfile(member).read(MAX_BUNDLE + 1))
    except (tarfile.TarError, OSError, ValueError) as error:
        raise Refused('invalid native bundle') from error


class NativeBundleSource:
    """read_bundle(source) supplies bytes; trusted descriptors pin SHA-256 out of band.

    A new extraction and Git projection on EVERY request. This transport boundary
    has local contract tests; no R2 credentials or network client are built in.
    """
    def __init__(self, binary, descriptors, read_bundle, authorized_threads=None):
        self.binary, self.descriptors, self.read_bundle = binary, dict(descriptors), read_bundle
        # Keep a private immutable copy; callers cannot expand an active grant list.
        self.authorized_threads = thread_grants(authorized_threads)

    def materialize(self, manifest, destination):
        source = manifest['source']
        allowed = self.authorized_threads.get(source)
        if allowed is not None and manifest['thread'] not in allowed:
            raise Refused('governing Thread is not authorized')
        expected = self.descriptors.get(source)
        if expected is None:
            raise Refused('missing native bundle')
        data = self.read_bundle(source)
        if (not isinstance(data, bytes) or len(data) > MAX_BUNDLE or
            hashlib.sha256(data).hexdigest() != expected):
            raise Refused('native bundle digest mismatch')
        with tempfile.TemporaryDirectory(prefix='heddle-bundle-') as temp:
            unpack_fixture(data, temp)
            NativeSource(self.binary, {source: temp},
                {key: list(value) for key, value in self.authorized_threads.items()}).materialize(manifest, destination)
