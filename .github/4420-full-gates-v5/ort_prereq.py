"""Portable fixed-asset ORT preparation; importing this file performs no work."""
import ctypes
import errno
import hashlib
import json
import os
import socket
from pathlib import Path, PurePosixPath
import tarfile
import time
import urllib.error
import urllib.request


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def write(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def verify_runtime(root, spec):
    for name, item in spec['members'].items():
        path = root / name
        if 'symlink' in item:
            if not path.is_symlink() or os.readlink(path) != item['symlink']:
                raise ValueError('ORT symlink changed: ' + name)
            if not path.resolve().is_relative_to(root.resolve()) or not path.resolve().is_file():
                raise ValueError('ORT symlink leaves the owned runtime: ' + name)
        elif path.is_symlink() or not path.is_file() or path.stat().st_size != item['bytes'] or sha(path) != item['sha256']:
            raise ValueError('ORT member changed: ' + name)
    actual = {str(path.relative_to(root)) for path in root.rglob('*') if path.is_file() or path.is_symlink()}
    if actual != set(spec['members']):
        raise ValueError('ORT runtime member set changed')


def retryable_download_error(error):
    """Retry transport failures only; integrity/extraction refusals stay final."""
    if isinstance(error, urllib.error.HTTPError):
        return error.code == 429 or 500 <= error.code < 600
    reason = error.reason if isinstance(error, urllib.error.URLError) else error
    if isinstance(reason, socket.gaierror):
        return reason.errno == socket.EAI_AGAIN
    return isinstance(reason, (TimeoutError, ConnectionError)) or (
        isinstance(reason, OSError) and reason.errno in {
            errno.ETIMEDOUT, errno.ECONNRESET, errno.ECONNABORTED,
            errno.ECONNREFUSED, errno.EHOSTUNREACH, errno.ENETUNREACH, errno.EPIPE,
        })


def download_archive(directory, spec):
    """One asset;3 attempts,180s budget plus <=60s in-flight socket overrun."""
    archive = directory / 'onnxruntime-linux-x64-1.24.4.tgz'
    started = time.monotonic()
    deadline = started + 180
    receipt = {'url': spec['url'], 'asset_id': spec['asset_id'],
               'archive_sha256_expected': spec['archive_sha256'],
               'archive_bytes_expected': spec['archive_bytes'],
               'max_attempts': 3, 'shared_deadline_seconds': 180,
               'request_timeout_seconds_max': 60, 'inflight_socket_overrun_seconds_max': 60,
               'transport_budget_including_socket_overrun_seconds': 240,
               'attempts': [], 'verified': False}
    receipt_path = directory / 'download-attempts.json'
    write(receipt_path, receipt)
    for number in range(1, 4):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            receipt.update(deadline_exceeded=True, elapsed_seconds=time.monotonic() - started)
            write(receipt_path, receipt)
            raise TimeoutError('official ORT download exceeded shared180s deadline')
        partial = directory / ('download-attempt-%d.part' % number)
        attempt = {'attempt': number, 'outcome': 'running', 'bytes_received': 0,
                   'request_timeout_seconds': min(60, remaining)}
        receipt['attempts'].append(attempt)
        write(receipt_path, receipt)
        digest = hashlib.sha256()
        size = 0
        try:
            request = urllib.request.Request(spec['url'], headers={'User-Agent': 'Nano-code-embed-fixed-prerequisite'})
            with urllib.request.urlopen(request, timeout=attempt['request_timeout_seconds']) as incoming, partial.open('xb') as stream:
                # read1 returns after one buffered/raw read rather than waiting
                # for a full chunk; check the shared deadline between reads.
                while True:
                    if time.monotonic() >= deadline:
                        raise TimeoutError('official ORT download exceeded shared180s deadline')
                    block = incoming.read1(256 * 1024)
                    if not block:
                        break
                    size += len(block)
                    if time.monotonic() > deadline:
                        raise TimeoutError('official ORT download exceeded shared180s deadline')
                    if size > spec['archive_bytes']:
                        raise ValueError('official ORT archive exceeds pinned length')
                    digest.update(block)
                    stream.write(block)
            if time.monotonic() > deadline:
                raise TimeoutError('official ORT download exceeded shared180s deadline')
            if size != spec['archive_bytes'] or digest.hexdigest() != spec['archive_sha256']:
                raise ValueError('official ORT release asset digest/length mismatch')
            partial.rename(archive)
            attempt.update(outcome='verified', bytes_received=size, archive_sha256=digest.hexdigest())
            receipt.update(verified=True, elapsed_seconds=time.monotonic() - started)
            write(receipt_path, receipt)
            return archive, size
        except Exception as error:
            if isinstance(error, urllib.error.HTTPError):
                error.close()
            if partial.exists():
                partial.unlink()
            retryable = retryable_download_error(error)
            remaining = deadline - time.monotonic()
            retry = retryable and number < 3 and remaining > 0
            attempt.update(outcome='failed', bytes_received=size, error_type=type(error).__name__,
                           http_status=error.code if isinstance(error, urllib.error.HTTPError) else None,
                           reason_type=type(error.reason).__name__ if isinstance(error, urllib.error.URLError) else None,
                           retryable_transport=retryable, retry_scheduled=retry)
            receipt['elapsed_seconds'] = time.monotonic() - started
            write(receipt_path, receipt)
            if not retry:
                raise
            time.sleep(min(2 ** (number - 1), remaining))
    raise AssertionError('bounded ORT download loop exhausted without outcome')


def prepare(directory, spec):
    """Download/verify/extract only; caller owns source/resource admission."""
    directory.mkdir(exist_ok=False)
    archive, size = download_archive(directory, spec)
    runtime = directory / 'runtime'
    runtime.mkdir()
    seen = set()
    links = []
    with tarfile.open(archive, 'r:gz') as packed:
        for member in packed.getmembers():
            name = str(PurePosixPath(member.name))
            parts = PurePosixPath(name).parts
            if not parts or parts[0] != spec['root'] or '..' in parts or name.startswith('/'):
                raise ValueError('invalid ORT archive member path')
            if member.isdir():
                if not any(item.startswith(name.rstrip('/') + '/') for item in spec['members']):
                    raise ValueError('unexpected ORT archive directory')
                continue
            if name in seen or name not in spec['members']:
                raise ValueError('duplicate/unexpected ORT archive member')
            seen.add(name)
            expected = spec['members'][name]
            output = runtime / name
            output.parent.mkdir(parents=True, exist_ok=True)
            if 'symlink' in expected:
                if not member.issym() or member.linkname != expected['symlink']:
                    raise ValueError('ORT archive link differs from verified release')
                links.append((output, member.linkname))
            else:
                if not member.isfile() or member.size != expected['bytes']:
                    raise ValueError('ORT archive file type/length mismatch')
                data = packed.extractfile(member).read()
                if hashlib.sha256(data).hexdigest() != expected['sha256']:
                    raise ValueError('ORT archive member hash mismatch')
                with output.open('xb') as stream:
                    stream.write(data)
                output.chmod(member.mode & 0o777)
    if seen != set(spec['members']):
        raise ValueError('incomplete ORT archive')
    for path, target in links:
        path.symlink_to(target)
    verify_runtime(runtime, spec)
    library = runtime / spec['library']
    libdir = library.parent
    cache = directory / 'model-cache'
    cache.mkdir()
    environment = {
        'ORT_LIB_PATH': str(libdir), 'ORT_PREFER_DYNAMIC_LINK': '1',
        'ORT_DYLIB_PATH': str(library), 'LD_LIBRARY_PATH': str(libdir),
        'HF_HOME': str(cache), 'FASTEMBED_CACHE_DIR': str(cache),
    }
    write(directory / 'preparation.json', {
        'url': spec['url'], 'asset_id': spec['asset_id'], 'archive_sha256': sha(archive),
        'archive_bytes': size, 'library_sha256': sha(library),
        'members_verified': len(seen), 'environment': environment,
        'load_or_inference_executed': False,
    })
    return runtime, environment


def load_api(runtime, spec):
    """Actual native loader proof, called only inside the CI bounded service."""
    verify_runtime(runtime, spec)
    library_path = runtime / spec['library']
    class ApiBase(ctypes.Structure):
        _fields_ = [('GetApi', ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_uint32)),
                    ('GetVersionString', ctypes.CFUNCTYPE(ctypes.c_char_p))]
    library = ctypes.CDLL(str(library_path))
    library.OrtGetApiBase.argtypes = []
    library.OrtGetApiBase.restype = ctypes.POINTER(ApiBase)
    base = library.OrtGetApiBase().contents
    version = base.GetVersionString().decode('ascii')
    if version != spec['version'] or not base.GetApi(spec['c_api']):
        raise ValueError('actual ORT version/C API mismatch')
    loaded = {}
    for line in Path('/proc/self/maps').read_text().splitlines():
        parts = line.split(maxsplit=5)
        if len(parts) == 6 and parts[-1].startswith('/'):
            path = Path(parts[-1])
            if path.is_file() and '.so' in path.name:
                loaded[str(path.resolve())] = sha(path.resolve())
    if loaded.get(str(library_path.resolve())) != spec['library_sha256']:
        raise ValueError('pinned ORT library is absent from actual loaded mappings')
    return {'version': version, 'c_api': spec['c_api'], 'load_and_api_pass': True,
            'library': str(library_path), 'library_sha256': sha(library_path),
            'loaded_shared_libraries': loaded, 'inference_executed': False}


def model_inventory(cache, spec):
    records = {}
    models = []
    for path in sorted(cache.rglob('*')):
        if not path.is_file() and not path.is_symlink():
            continue
        resolved = path.resolve()
        if not resolved.is_relative_to(cache.resolve()) or not resolved.is_file():
            raise ValueError('model cache entry leaves its owned directory')
        relative = str(path.relative_to(cache))
        item = {'sha256': sha(resolved), 'bytes': resolved.stat().st_size}
        if path.is_symlink():
            item['symlink'] = os.readlink(path)
        records[relative] = item
        if spec['model_repo'] in path.relative_to(cache).parts and relative.endswith(spec['model_file_suffix']):
            if not item['bytes']:
                raise ValueError('empty actual ONNX model')
            models.append(relative)
    return {'files': records, 'default_model_onnx_files': models,
            'model_repo': spec['model_repo'], 'dimension_expected': spec['model_dimension']}
