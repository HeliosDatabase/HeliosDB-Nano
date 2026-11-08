"""Small synchronous, trust-only PostgreSQL v3 codec for the native CI probe.

stdlib only, Python3.9+. No connection/process starts at import time. The caller
owns the daemon/port/source proof and must check every result with require_ok().
Server SQL errors are drained through ReadyForQuery and returned structurally;
transport, framing, unsupported-auth and deadline failures close the connection.
BackendKeyData bodies are never retained: public frames contain type/length
metadata only for complete or partial K messages. No CancelRequest is supported.
"""
import base64
from dataclasses import dataclass, field
from enum import Enum
import math
import re
import socket
import struct
import time
from typing import Dict, List, Optional


class ReadyTxnStatus(Enum):
    IDLE = "I"
    IN_TRANSACTION = "T"
    FAILED_TRANSACTION = "E"


@dataclass
class PgError:
    severity: Optional[str]
    sqlstate: Optional[str]
    message: Optional[str]
    fields: Dict[str, str]

    def to_receipt(self):
        return {"severity": self.severity, "sqlstate": self.sqlstate,
                "message": self.message, "fields": dict(self.fields)}


@dataclass
class PgCommand:
    columns: List[dict] = field(default_factory=list)
    rows: List[List[Optional[str]]] = field(default_factory=list)
    tag: Optional[str] = None
    complete: bool = False

    def to_receipt(self):
        return {"columns": self.columns, "rows": self.rows,
                "command_tag": self.tag, "complete": self.complete}


@dataclass
class PgResult:
    protocol: str
    sql: Optional[str]
    commands: List[PgCommand] = field(default_factory=list)
    errors: List[PgError] = field(default_factory=list)
    notices: List[PgError] = field(default_factory=list)
    events: List[dict] = field(default_factory=list)
    frames: List[dict] = field(default_factory=list)
    ready: Optional[ReadyTxnStatus] = None
    completed: bool = False
    elapsed_s: Optional[float] = None
    frame_bytes: int = 0

    @property
    def rows(self):
        if len(self.commands) > 1:
            raise ValueError("multiple SQL command results; inspect commands explicitly")
        return self.commands[0].rows if self.commands else []

    @property
    def columns(self):
        if len(self.commands) > 1:
            raise ValueError("multiple SQL command results; inspect commands explicitly")
        return self.commands[0].columns if self.commands else []

    @property
    def error(self):
        return self.errors[0] if self.errors else None

    def require_ok(self):
        if self.errors:
            raise PgServerError(self)
        if not self.completed or self.ready is None:
            raise ValueError("exchange has no complete ReadyForQuery")
        return self

    def to_receipt(self):
        return {"protocol": self.protocol, "sql": self.sql,
                "commands": [x.to_receipt() for x in self.commands],
                "errors": [x.to_receipt() for x in self.errors],
                "notices": [x.to_receipt() for x in self.notices],
                "events": self.events, "raw_frames": self.frames,
                "raw_frame_bytes": self.frame_bytes,
                "ready_transaction_status": self.ready.value if self.ready else None,
                "ready_transaction_status_name": self.ready.name if self.ready else None,
                "completed": self.completed, "elapsed_s": self.elapsed_s}


class PgServerError(RuntimeError):
    """A complete SQL ErrorResponse/Ready exchange; connection remains usable."""
    def __init__(self, result):
        self.result = result
        self.receipt = result.to_receipt()
        error = result.errors[0]
        super().__init__("PG {}: {}".format(error.sqlstate, error.message))


class PgWireFailure(RuntimeError):
    """Connection is unusable; receipt retains complete/partial attempted frames."""
    def __init__(self, message, result):
        self.result = result
        self.receipt = result.to_receipt()
        super().__init__(message)


class _Truncated(Exception):
    def __init__(self, message, partial, received_count=None):
        self.partial = partial
        self.received_count = len(partial) if received_count is None else received_count
        super().__init__(message)


def _cstring(value):
    raw = value.encode("utf-8")
    if b"\0" in raw:
        raise ValueError("NUL in PostgreSQL string")
    return raw + b"\0"


def _take_cstring(payload, offset=0):
    end = payload.find(b"\0", offset)
    if end < 0:
        raise ValueError("unterminated PostgreSQL string")
    return payload[offset:end].decode("utf-8"), end + 1


def _single_cstring(payload):
    text, end = _take_cstring(payload)
    if end != len(payload):
        raise ValueError("trailing bytes after PostgreSQL string")
    return text


def _pg_error(payload, strict=True):
    fields = {}
    offset = 0
    while offset < len(payload) and payload[offset] != 0:
        code = chr(payload[offset]); offset += 1
        if code in fields:
            raise ValueError("duplicate ErrorResponse field")
        fields[code], offset = _take_cstring(payload, offset)
    if offset != len(payload) - 1 or payload[offset:] != b"\0":
        raise ValueError("malformed ErrorResponse terminator")
    state = fields.get("C")
    if strict and (not state or not re.fullmatch(r"[A-Z0-9]{5}", state) or "M" not in fields):
        raise ValueError("ErrorResponse lacks SQLSTATE/message")
    return PgError(fields.get("V", fields.get("S")), state, fields.get("M"), fields)


def _row_description(payload):
    if len(payload) < 2:
        raise ValueError("short RowDescription")
    count = struct.unpack_from("!H", payload)[0]
    if count > 1024:
        raise ValueError("probe RowDescription exceeds1024 columns")
    offset = 2; columns = []
    for _ in range(count):
        name, offset = _take_cstring(payload, offset)
        if offset + 18 > len(payload):
            raise ValueError("short RowDescription field")
        table, attr, oid, size, modifier, fmt = struct.unpack_from("!IhIhih", payload, offset)
        offset += 18
        if fmt != 0:
            raise ValueError("probe requested text; server described binary field")
        columns.append({"name": name, "table_oid": table, "attribute_number": attr,
                        "type_oid": oid, "type_size": size, "type_modifier": modifier,
                        "format_code": fmt})
    if offset != len(payload):
        raise ValueError("trailing RowDescription bytes")
    return columns


def _data_row(payload, width):
    if len(payload) < 2:
        raise ValueError("short DataRow")
    count = struct.unpack_from("!H", payload)[0]
    if count != width:
        raise ValueError("DataRow arity differs from RowDescription")
    offset = 2; row = []
    for _ in range(count):
        if offset + 4 > len(payload):
            raise ValueError("short DataRow length")
        length = struct.unpack_from("!i", payload, offset)[0]; offset += 4
        if length == -1:
            row.append(None)
        elif length < 0 or offset + length > len(payload):
            raise ValueError("invalid/truncated DataRow field")
        else:
            row.append(payload[offset:offset + length].decode("utf-8")); offset += length
    if offset != len(payload):
        raise ValueError("trailing DataRow bytes")
    return row


class PgWireClient:
    MAX_FRAME = 16 * 1024 * 1024
    MAX_EXCHANGE = 64 * 1024 * 1024

    def __init__(self, connected_socket, timeout=8.0):
        if isinstance(timeout, bool) or not math.isfinite(timeout) or not 0 < timeout <= 8:
            raise ValueError("per-exchange deadline must be positive and at most8s")
        self.socket = connected_socket
        self.timeout = float(timeout)
        self.closed = False
        self.busy = False
        self.ready = None
        self.startup_result = None
        self.last_result = None
        self.parameters = {}
        self.backend_pid = None
        self.close_error = None

    @classmethod
    def connect(cls, host, port, user="heliosdb", database="postgres", timeout=8.0):
        # The owning worker must first prove this loopback port belongs to its
        # just-spawned daemon. No DNS, TLS, password or authentication fallback.
        if host != "127.0.0.1" or type(port) is not int or not 0 < port < 65536:
            raise ValueError("probe requires exact IPv4 loopback and allocated port")
        if isinstance(timeout, bool) or not math.isfinite(timeout) or not 0 < timeout <= 8:
            raise ValueError("startup deadline must be positive and at most8s")
        started = time.monotonic()
        try:
            sock = socket.create_connection((host, port), timeout=timeout)
        except OSError as error:
            result = PgResult("startup", None, elapsed_s=time.monotonic() - started)
            result.events.append({"kind": "WireFailure", "exception": type(error).__name__, "message": str(error)})
            raise PgWireFailure(str(error), result) from error
        client = cls(sock, timeout)
        try:
            client.startup_result = client._startup(user, database, started + timeout)
            return client
        except Exception:
            client.close()
            raise

    def close(self):
        if not self.closed:
            self.closed = True
            try:
                self.socket.close()
            except OSError as error:
                self.close_error = str(error)

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        self.close()

    def _timeout(self, deadline):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("PostgreSQL exchange deadline exceeded")
        self.socket.settimeout(remaining)

    def _record(self, result, direction, kind, raw, complete, byte_length=None):
        byte_length = len(raw) if byte_length is None else byte_length
        if result.frame_bytes + byte_length > self.MAX_EXCHANGE or len(result.frames) >= 65536:
            raise ValueError("probe exchange exceeds64MiB receipt bound")
        row = {"direction": direction, "type": kind, "byte_length": byte_length,
               "complete": complete}
        if direction == "backend" and raw[:1] == b"K":
            # K carries a live cancellation secret. Keep NO raw header/body,
            # body digest, decoded secret or partial bytes in public evidence.
            row["type"] = "K"
            row["redacted"] = "BackendKeyData body omitted"
            if len(raw) >= 5:
                row["declared_byte_length"] = struct.unpack("!I", raw[1:5])[0] + 1
        else:
            row["bytes_b64"] = base64.b64encode(raw).decode("ascii")
        result.frames.append(row)
        result.frame_bytes += byte_length
        return row

    def _send(self, result, kind, payload, deadline):
        if kind is None:
            raw = struct.pack("!I", len(payload) + 4) + payload
        else:
            raw = kind + struct.pack("!I", len(payload) + 4) + payload
        if len(raw) > self.MAX_FRAME:
            raise ValueError("probe frontend frame exceeds16MiB")
        event = self._record(result, "frontend", kind.decode("ascii") if kind else "StartupMessage", raw, False)
        self._timeout(deadline)
        self.socket.sendall(raw)
        event["complete"] = True

    def _read(self, count, deadline, sensitive=False):
        raw = bytearray()
        chunk = b""
        try:
            while len(raw) < count:
                self._timeout(deadline)
                chunk = self.socket.recv(count - len(raw))
                if not chunk:
                    raise OSError("EOF in PostgreSQL frame")
                raw.extend(chunk)
        except (OSError, TimeoutError) as error:
            if sensitive:
                received = len(raw)
                raw.clear(); chunk = b""
                raise _Truncated(str(error), b"", received) from error
            raise _Truncated(str(error), bytes(raw)) from error
        return bytes(raw)

    def _receive(self, result, deadline):
        try:
            header = self._read(5, deadline)
        except _Truncated as error:
            if error.partial:
                self._record(result, "backend", "partial-header", error.partial, False)
                if error.partial[:1] == b"K":
                    error.partial = b""
            raise
        kind = header[:1]
        length = struct.unpack("!I", header[1:])[0]
        if not 4 <= length <= self.MAX_FRAME - 1:
            self._record(result, "backend", kind.hex(), header, False)
            raise ValueError("invalid/oversized backend frame length")
        if kind == b"K" and length != 12:
            self._record(result, "backend", "K", header, False)
            raise ValueError("malformed BackendKeyData length")
        try:
            payload = self._read(length - 4, deadline, sensitive=kind == b"K")
        except _Truncated as error:
            if kind == b"K":
                self._record(result, "backend", "K", header, False, 5 + error.received_count)
                error.partial = b""
            else:
                self._record(result, "backend", kind.hex(), header + error.partial, False)
            raise
        if kind == b"K":
            # Retain only the permitted PID, before any event/exception can
            # retain a payload. Never decode/store the cancellation secret.
            pid_payload = payload[:4]
            payload = b""
            self._record(result, "backend", "K", header, True, 13)
            return kind, pid_payload
        self._record(result, "backend", kind.decode("ascii"), header + payload, True)
        return kind, payload

    def _shared_message(self, result, kind, payload):
        if kind == b"S":
            name, offset = _take_cstring(payload)
            value, end = _take_cstring(payload, offset)
            if end != len(payload):
                raise ValueError("trailing ParameterStatus bytes")
            self.parameters[name] = value
            result.events.append({"kind": "ParameterStatus", "name": name, "value": value})
        elif kind == b"K":
            # _receive already verified full8-byte body and stripped secret.
            if len(payload) != 4:
                raise ValueError("malformed BackendKeyData")
            self.backend_pid = struct.unpack_from("!I", payload)[0]
            result.events.append({"kind": "BackendKeyData", "backend_pid": self.backend_pid})
        elif kind == b"N":
            result.notices.append(_pg_error(payload, strict=False))
        elif kind == b"A":
            if len(payload) < 4:
                raise ValueError("short NotificationResponse")
            pid = struct.unpack_from("!I", payload)[0]
            channel, offset = _take_cstring(payload, 4)
            value, end = _take_cstring(payload, offset)
            if end != len(payload):
                raise ValueError("trailing NotificationResponse bytes")
            result.events.append({"kind": "NotificationResponse", "backend_pid": pid,
                                  "channel": channel, "payload": value})
        else:
            return False
        return True

    def _exchange(self, result, outgoing, startup=False, deadline=None):
        if self.closed or self.busy:
            raise ValueError("connection closed or concurrent use attempted")
        self.busy = True
        started = time.monotonic()
        deadline = started + self.timeout if deadline is None else deadline
        active = None; authenticated = False; counts = {b"1": 0, b"2": 0}; descriptions = 0
        try:
            for kind, payload in outgoing:
                self._send(result, kind, payload, deadline)
            # Extended Sync is ALREADY in outgoing. On ErrorResponse, drain
            # through its one ReadyForQuery; sending an extra Sync would leave
            # an extra Ready queued and corrupt the next request's observation.
            while True:
                kind, payload = self._receive(result, deadline)
                if self._shared_message(result, kind, payload):
                    continue
                if kind == b"E":
                    if result.errors:
                        raise ValueError("multiple ErrorResponses before one ReadyForQuery")
                    result.errors.append(_pg_error(payload))
                    if active is not None:
                        result.commands.append(active); active = None
                elif kind == b"Z":
                    if len(payload) != 1:
                        raise ValueError("malformed ReadyForQuery")
                    result.ready = ReadyTxnStatus(payload.decode("ascii"))
                    if active is not None:
                        raise ValueError("RowDescription has no CommandComplete/error")
                    break
                elif startup and kind == b"R":
                    if payload != struct.pack("!I", 0) or authenticated:
                        raise ValueError("trust-only probe requires one AuthenticationOk(code0)")
                    authenticated = True
                    result.events.append({"kind": "AuthenticationOk", "auth_code": 0})
                elif startup:
                    raise ValueError("unexpected startup backend message: " + repr(kind))
                elif result.errors:
                    raise ValueError("unexpected command response after ErrorResponse")
                elif kind == b"T":
                    if active is not None or (result.protocol == "extended" and descriptions):
                        raise ValueError("duplicate/uncompleted RowDescription")
                    descriptions += 1
                    active = PgCommand(columns=_row_description(payload))
                elif kind == b"D":
                    if active is None:
                        raise ValueError("DataRow without RowDescription")
                    active.rows.append(_data_row(payload, len(active.columns)))
                elif kind == b"C":
                    command = active if active is not None else PgCommand()
                    command.tag = _single_cstring(payload); command.complete = True
                    result.commands.append(command); active = None
                elif kind == b"I":
                    if payload or active is not None:
                        raise ValueError("malformed EmptyQueryResponse")
                    result.commands.append(PgCommand(tag="", complete=True))
                elif kind in (b"1", b"2"):
                    if result.protocol != "extended" or payload:
                        raise ValueError("unexpected/nonempty extended completion")
                    counts[kind] += 1
                    if counts[kind] != 1:
                        raise ValueError("duplicate extended completion")
                elif kind == b"n":
                    if result.protocol != "extended" or payload or descriptions:
                        raise ValueError("unexpected/duplicate NoData")
                    descriptions += 1
                else:
                    raise ValueError("unsupported backend response: " + repr(kind))
            if startup:
                if not authenticated or result.errors or result.ready is not ReadyTxnStatus.IDLE:
                    raise ValueError("startup did not reach authenticated idle ReadyForQuery")
            elif not result.errors:
                if not result.commands:
                    raise ValueError("ReadyForQuery arrived before any command result")
                if result.protocol == "extended" and (counts != {b"1": 1, b"2": 1} or descriptions != 1 or len(result.commands) != 1):
                    raise ValueError("incomplete extended Parse/Bind/Describe/Execute exchange")
            self.ready = result.ready
            result.completed = True
            return result
        except (OSError, TimeoutError, ValueError, _Truncated) as error:
            self.close()
            result.elapsed_s = time.monotonic() - started
            result.events.append({"kind": "WireFailure", "exception": type(error).__name__, "message": str(error)})
            raise PgWireFailure(str(error), result) from error
        finally:
            self.busy = False
            result.elapsed_s = time.monotonic() - started
            self.last_result = result

    def _startup(self, user, database, deadline=None):
        payload = struct.pack("!I", 196608) + _cstring("user") + _cstring(user)
        payload += _cstring("database") + _cstring(database)
        payload += _cstring("client_encoding") + _cstring("UTF8")
        payload += _cstring("application_name") + _cstring("nano-routine-txn-ci") + b"\0"
        return self._exchange(PgResult("startup", None), [(None, payload)], startup=True, deadline=deadline)

    def simple(self, sql):
        return self._exchange(PgResult("simple", sql), [(b"Q", _cstring(sql))])

    def extended(self, sql):
        # Empty parameter list; all result columns explicitly text format.
        parse = b"\0" + _cstring(sql) + struct.pack("!H", 0)
        bind = b"\0\0" + struct.pack("!HHH", 0, 0, 0)
        describe_portal = b"P\0"
        execute = b"\0" + struct.pack("!I", 0)
        return self._exchange(PgResult("extended", sql),
                              [(b"P", parse), (b"B", bind), (b"D", describe_portal),
                               (b"E", execute), (b"S", b"")])

# Native semantic controller. The codec above is source-held at 037e1582...313fb.
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import tarfile
import tempfile
import traceback

PRODUCER_HEAD = 'ae46e4b95ed578efa9d543809b4ea67e64096b68'
PRODUCER_RUN = 42462448705
PROVIDER_ZIP_SHA = 'a606306760e2e2b30a921a5eb58f3b5cfc9fe953d78a1398341a20018006bc83'
BINARY_SHA = '5116cee940ca93e0a63dd84ba27792a609a9746a841b46c1132481df2153c45f'
ARCHIVE = 'heliosdb-nano-v4.42.0-x86_64-unknown-linux-gnu.tar.gz'
MODEL = '.github/binary-preflight-source28.json'
LEGACY = {
    MODEL: 'ed47e15814dd3d39b072f5fb98aa632b85cd4b11177277e2796aee7cf3991038',
    '.github/scripts/binary_preflight.py': 'ea23902c06733505a8cbf093f61d88c3f93162fc48f415257b76d3b319a8fa84',
    '.github/workflows/binary-preflight.yml': 'd19d830c9e2797567e9828bcf1bbfc4cccda6ac3e50096c0b60628e48c35de2b',
}
NEW = {'.github/scripts/routine_txn_native.py', '.github/workflows/routine-txn-native.yml'}
PAYLOAD = {
    'SHA256SUMS': '4998d534c7193d91744b87fd057822f03c4113513034cb376d3f5693eb767187',
    'aggregate-source.json': 'e06dd4e39c60a6496f12926342baa42073a55068b70bb85a468a8d7dcd9f32c6',
    'heliosdb-nano-v4.42.0-aarch64-apple-darwin.tar.gz': '2397e3774e2a79b9509c4dd65b5098386ba589df0107345ead9cbd711a869d72',
    'heliosdb-nano-v4.42.0-aarch64-unknown-linux-gnu.tar.gz': '5e7f21d0934ece2ec292e96410c399b3b8c17a46d6444433e421ad789f3d61e2',
    'heliosdb-nano-v4.42.0-x86_64-pc-windows-msvc.zip': 'f40bfd7c473d92a19e49c9551b49504d137bd0448f96bfe9d0677799d74aa7d0',
    ARCHIVE: 'a5c4572ca94f050978541eb16f7be7b2fffd1bd51c1d13034bef4e8a1748e160',
    'summary.json': '959e6432d6d5eda26c04e7e971499ac6fc5ec260ac2ffb1f655672b0aed066ba',
}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args], timeout=15)


def source_check(root):
    head = git(root, 'rev-parse', 'HEAD').decode().strip()
    if head != os.environ.get('GITHUB_SHA'):
        raise ValueError('checkout HEAD differs from event SHA')
    if git(root, 'diff', '--name-only', 'HEAD') or git(root, 'diff', '--cached', '--name-only'):
        raise ValueError('tracked source changed')
    for name, digest in LEGACY.items():
        if sha((root / name).read_bytes()) != digest:
            raise ValueError('legacy auxiliary hash mismatch: ' + name)
    model = json.loads((root / MODEL).read_text())
    expected = model['entries']
    if len(expected) != 4049 or model['regular_files'] != 4045:
        raise ValueError('unexpected canonical source count')
    entries = {}
    for record in git(root, 'ls-tree', '-rz', '--full-tree', 'HEAD').split(b'\0'):
        if not record:
            continue
        info, name = record.split(b'\t', 1)
        mode, kind, oid = info.decode().split()
        if kind != 'blob':
            raise ValueError('non-blob committed tree entry')
        entries[name.decode()] = {'mode': mode, 'oid': oid}
    if set(entries) != set(expected) | set(LEGACY) | NEW or len(entries) != 4054:
        raise ValueError('unexpected committed paths')
    physical = {}
    for name, record in expected.items():
        if entries[name] != {k: record[k] for k in ('mode', 'oid')}:
            raise ValueError('canonical Git blob/mode changed: ' + name)
        path = root / name
        if record['mode'] == '120000':
            if not path.is_symlink() or os.readlink(path) != record['target_text']:
                raise ValueError('canonical symlink target changed: ' + name)
        else:
            if path.is_symlink() or not path.is_file():
                raise ValueError('regular source missing or is symlink: ' + name)
            physical[name] = sha(path.read_bytes())
            if physical[name] != record['sha256']:
                raise ValueError('physical source bytes changed: ' + name)
    for name in set(LEGACY) | NEW:
        if entries[name]['mode'] != '100644' or (root / name).is_symlink():
            raise ValueError('invalid auxiliary mode: ' + name)
        if git(root, 'show', 'HEAD:' + name) != (root / name).read_bytes():
            raise ValueError('physical auxiliary differs from committed blob: ' + name)
    unknown = git(root, 'ls-files', '--others', '--exclude-standard', '-z').split(b'\0')
    if any(x and not x.decode().startswith(('.routine-artifacts/', '.routine-txn-out/')) for x in unknown):
        raise ValueError('unexpected untracked file')
    return {'head': head, 'git_entries': entries,
            'regular_file_count': len(physical), 'source_input_count': len(expected),
            'physical_sha256_digest': sha(json.dumps(physical, sort_keys=True).encode()),
            'model_sha256': LEGACY[MODEL], 'product_source': 'source31',
            'extra_auxiliary_paths': sorted(set(LEGACY) | NEW)}


def artifact_check(root, incoming, out, source):
    names = {p.name for p in incoming.iterdir() if p.is_file()}
    if names != set(PAYLOAD) or any(p.is_symlink() or not p.is_file() for p in incoming.iterdir()):
        raise ValueError('fixed artifact payload must contain exactly seven regular files')
    for name, digest in PAYLOAD.items():
        if sha((incoming / name).read_bytes()) != digest:
            raise ValueError('fixed artifact hash mismatch: ' + name)
    aggregate = json.loads((incoming / 'aggregate-source.json').read_text())
    summary = json.loads((incoming / 'summary.json').read_text())
    old_entries = {k: v for k, v in source['git_entries'].items() if k not in NEW}
    if (aggregate['head'] != PRODUCER_HEAD or not aggregate['success']
            or aggregate['git_entries'] != old_entries
            or aggregate['source_input_count'] != 4049 or aggregate['regular_file_count'] != 4045
            or aggregate['expected_manifest_sha256'] != LEGACY[MODEL]
            or summary['head'] != PRODUCER_HEAD or not summary['success']
            or summary['target_count'] != 4 or len(summary['targets']) != 4):
        raise ValueError('producer source/aggregate identity mismatch')
    target = next(x for x in summary['targets'] if x['target'] == 'x86_64-unknown-linux-gnu')
    if target['binary_sha256'] != BINARY_SHA or target['sha256'] != PAYLOAD[ARCHIVE]:
        raise ValueError('producer native identity mismatch')
    sums = dict((name, digest) for digest, name in
                (line.split() for line in (incoming / 'SHA256SUMS').read_text().splitlines()))
    if sums != {x['archive']: x['sha256'] for x in summary['targets']}:
        raise ValueError('aggregate checksum map differs')
    with tarfile.open(incoming / ARCHIVE, 'r:gz') as archive:
        members = archive.getmembers()
        if {x.name for x in members} != {'heliosdb-nano', 'LICENSE', 'README.md'} or len(members) != 3 or any(not x.isfile() for x in members):
            raise ValueError('unexpected native archive members')
        raw = archive.extractfile('heliosdb-nano').read()
    if sha(raw) != BINARY_SHA:
        raise ValueError('native binary digest mismatch')
    binary = out / 'heliosdb-nano'
    binary.write_bytes(raw)
    binary.chmod(0o755)
    version = subprocess.run([str(binary), '--version'], capture_output=True, timeout=8)
    if version.returncode != 0 or version.stdout.decode().strip() != 'heliosdb-nano 4.42.0':
        raise ValueError('native version mismatch: ' + repr(version.stdout))
    write_json(out / 'artifact-identity.json', {
        'producer_run': PRODUCER_RUN, 'producer_head': PRODUCER_HEAD,
        'provider_artifact_id': 13610852764, 'provider_zip_sha256_root_verified': PROVIDER_ZIP_SHA,
        'provider_zip_recomputed_here': False, 'download_action_payload_sha256': PAYLOAD,
        'native_sha256': BINARY_SHA, 'version_stdout': version.stdout.decode(),
        'version_stderr': version.stderr.decode(), 'version_exit': version.returncode,
        'source_join': '4049 canonical Git entries and 4045 physical inputs identical to producer; five declared auxiliaries only',
    })
    return binary


def birth(pid):
    return int(Path('/proc/{}/stat'.format(pid)).read_text().rsplit(')', 1)[1].split()[19])


def owned(proc, started, argv, binary):
    return (proc.poll() is None and birth(proc.pid) == started
            and Path('/proc/{}/cmdline'.format(proc.pid)).read_bytes().split(b'\0')[:-1] == [os.fsencode(x) for x in argv]
            and Path('/proc/{}/exe'.format(proc.pid)).resolve() == binary.resolve())


def owns_listener(pid, port):
    inodes = set()
    for fd in Path('/proc/{}/fd'.format(pid)).iterdir():
        try:
            link = os.readlink(fd)
            if link.startswith('socket:['):
                inodes.add(link[8:-1])
        except FileNotFoundError:
            pass
    return any(len(v) > 9 and v[1] == '0100007F:{:04X}'.format(port) and v[3] == '0A' and v[9] in inodes
               for v in (line.split() for line in Path('/proc/net/tcp').read_text().splitlines()[1:]))


def cleanup(proc, started, argv, binary):
    if proc.poll() is not None:
        proc.wait()
        return {'owned': True, 'already_exited': True, 'exit': proc.returncode}
    if not owned(proc, started, argv, binary):
        raise RuntimeError('refusing cleanup: child identity changed')
    proc.terminate()
    try:
        proc.wait(timeout=15)
        return {'owned': True, 'term': True, 'exit': proc.returncode}
    except subprocess.TimeoutExpired:
        if not owned(proc, started, argv, binary):
            raise RuntimeError('refusing KILL: child identity changed')
        proc.kill()
        proc.wait(timeout=10)
        return {'owned': True, 'term': True, 'kill': True, 'exit': proc.returncode}


def run_cell(binary, output, history, family, kind):
    name = '{}-{}-{}'.format('tt' if history else 'no-tt', family, kind)
    folder = output / name
    folder.mkdir()
    cell = {'name': name, 'time_travel_enabled': history, 'family': family, 'kind': kind,
            'status': 'failed', 'exchanges': [], 'mismatches': [], 'premises': {}, 'wire_raw_k_redacted': True}
    clients = []
    proc = None
    start_ticks = None
    argv = None
    deadline = time.monotonic() + 60

    def connect(label):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('cell deadline')
        if not owned(proc, start_ticks, argv, binary) or not owns_listener(proc.pid, port):
            raise RuntimeError('refusing connection to unowned endpoint: ' + label)
        client = PgWireClient.connect('127.0.0.1', port, user='heliosdb', database='heliosdb', timeout=min(8, remaining))
        clients.append(client)
        cell['exchanges'].append({'phase': label + ':startup', 'receipt': client.startup_result.to_receipt()})
        if not owned(proc, start_ticks, argv, binary) or not owns_listener(proc.pid, port):
            client.close()
            raise RuntimeError('endpoint ownership changed during startup: ' + label)
        return client

    def query(client, sql, label, rows=None, status='I', protocol=None, fatal=True):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('cell deadline before ' + label)
        client.timeout = min(8, remaining)
        try:
            result = getattr(client, protocol or family)(sql)
        except PgWireFailure as error:
            cell['exchanges'].append({'phase': label, 'receipt': error.receipt})
            raise
        cell['exchanges'].append({'phase': label, 'receipt': result.to_receipt()})
        matches = not result.errors and result.completed and result.ready is not None and result.ready.value == status
        if rows is not None:
            matches = matches and result.rows == rows
        if not matches:
            cell['mismatches'].append({'phase': label, 'expected_rows': rows, 'expected_ready': status,
                                       'actual_rows': result.rows, 'actual_ready': result.ready.value if result.ready else None,
                                       'sql_errors': [x.to_receipt() for x in result.errors]})
            if fatal:
                raise AssertionError('premise failed: ' + label)
        return matches

    try:
        with tempfile.TemporaryDirectory(prefix='nano-routine-') as temp:
            work = Path(temp)
            config = ('[storage]\ntime_travel_enabled = {}\nversion_gc_interval_secs = 0\n'
                      'logical_wal_per_statement = false\nwal_checkpoint_interval_entries = 0\n'
                      'wal_checkpoint_interval_secs = 0\n').format(str(history).lower())
            (work / 'config.toml').write_text(config)
            (folder / 'config.toml').write_text(config)
            reservation = socket.socket()
            reservation.bind(('127.0.0.1', 0))
            port = reservation.getsockname()[1]
            argv = [str(binary), 'start', '--data-dir', str(work / 'data'), '--listen', '127.0.0.1',
                    '--port', str(port), '--auth', 'trust', '--http-port', '0', '--config', str(work / 'config.toml')]
            env = {k: os.environ[k] for k in ('PATH', 'HOME', 'USER', 'LOGNAME', 'LANG') if k in os.environ}
            env.update(TZ='UTC', RUST_LOG='warn', HELIOSDB_NANO_STATEMENT_OFFLOAD='on')
            with (folder / 'server.stdout').open('wb') as stdout, (folder / 'server.stderr').open('wb') as stderr:
                reservation.close()
                proc = subprocess.Popen(argv, cwd=work, env=env, stdout=stdout, stderr=stderr)
                start_ticks = birth(proc.pid)
                cell['server'] = {'pid': proc.pid, 'birth_ticks': start_ticks, 'argv': argv, 'environment_keys': sorted(env)}
                try:
                    ready_end = time.monotonic() + 8
                    while time.monotonic() < ready_end:
                        if proc.poll() is not None:
                            raise RuntimeError('daemon exited before READY: ' + str(proc.returncode))
                        if owned(proc, start_ticks, argv, binary) and owns_listener(proc.pid, port):
                            break
                        time.sleep(.05)
                    else:
                        raise TimeoutError('owned daemon listener readiness exceeded8s')
                    cell['server']['listener_owned'] = True
                    seed = connect('seed-main')
                    statements = [
                        'CREATE TABLE rb_source (id INT PRIMARY KEY)',
                        'CREATE TABLE rb_audit (id INT PRIMARY KEY)',
                        'INSERT INTO rb_source VALUES (1)', 'INSERT INTO rb_audit VALUES (101)',
                        'CREATE FUNCTION rb_count() RETURNS BIGINT AS $$ SELECT COUNT(*) FROM rb_source $$ LANGUAGE sql',
                        'CREATE FUNCTION rb_log(n INT) RETURNS INT AS $$ BEGIN INSERT INTO rb_audit VALUES ($1); RETURN $1; END $$ LANGUAGE plpgsql',
                        'CREATE PROCEDURE rb_copy() LANGUAGE sql AS $$ INSERT INTO rb_audit SELECT id + 1000 FROM rb_source $$',
                        'CREATE DATABASE BRANCH rb_child FROM main AS OF NOW',
                    ]
                    for n, sql in enumerate(statements):
                        query(seed, sql, 'seed-main:' + str(n), protocol='simple')
                    branch_seed = connect('seed-branch')
                    for n, sql in enumerate(('USE BRANCH rb_child', 'INSERT INTO rb_source VALUES (2)', 'INSERT INTO rb_audit VALUES (202)')):
                        query(branch_seed, sql, 'seed-branch:' + str(n), protocol='simple')
                    seed.close()
                    branch_seed.close()
                    caller = connect('caller')
                    query(caller, 'USE BRANCH rb_child', 'caller:branch')
                    cell['premises']['committed_branch_function_count_two'] = query(caller, 'SELECT rb_count()', 'caller:committed-count', [['2']], fatal=False)
                    query(caller, 'BEGIN', 'caller:begin', status='T')
                    query(caller, 'INSERT INTO rb_source VALUES (3)', 'caller:stage-three', status='T')
                    query(caller, 'SELECT id FROM rb_source ORDER BY id', 'caller:staged-source', [['1'], ['2'], ['3']], 'T')
                    cell['premises']['staged_source_ready_t'] = True
                    target = {'read': ('SELECT rb_count()', [['3']], [['101'], ['202']]),
                              'write': ('SELECT rb_log(303)', [['303']], [['101'], ['202'], ['303']]),
                              'call': ('CALL rb_copy()', [], [['101'], ['202'], ['1001'], ['1002'], ['1003']])}[kind]
                    cell['target_attempted'] = True
                    query(caller, target[0], 'target:' + kind, target[1], 'T', fatal=False)
                    query(caller, 'SELECT id FROM rb_audit ORDER BY id', 'target:audit', target[2], 'T', fatal=False)
                    query(caller, 'ROLLBACK', 'caller:rollback', status='I', fatal=False)
                    caller.close()
                    for branch, expected_source, expected_audit in (
                        ('main', [['1']], [['101']]),
                        ('rb_child', [['1'], ['2']], [['101'], ['202']]),
                    ):
                        fresh = connect('fresh-' + branch)
                        if branch != 'main':
                            query(fresh, 'USE BRANCH rb_child', 'fresh:branch', protocol='simple')
                        query(fresh, 'SELECT id FROM rb_source ORDER BY id', 'fresh-' + branch + ':source', expected_source, protocol='simple', fatal=False)
                        query(fresh, 'SELECT id FROM rb_audit ORDER BY id', 'fresh-' + branch + ':audit', expected_audit, protocol='simple', fatal=False)
                        fresh.close()
                    cell['post_rollback_oracles_completed'] = True
                    if not cell['mismatches']:
                        cell['status'] = 'passed'
                finally:
                    for client in clients:
                        client.close()
                    cell['cleanup'] = cleanup(proc, start_ticks, argv, binary)
                    proc = None
    except BaseException as error:
        cell['exception'] = {'type': type(error).__name__, 'message': str(error)}
        if hasattr(error, 'receipt'):
            cell['exception']['receipt'] = error.receipt
        cell['status'] = 'failed'
        if isinstance(error, (KeyboardInterrupt, SystemExit)):
            raise
    finally:
        if proc is not None:
            cell['cleanup'] = cleanup(proc, start_ticks, argv, binary)
        write_json(folder / 'cell.json', cell)
    print('NATIVE_ROUTINE_CELL ' + name + ' ' + cell['status'], flush=True)
    return cell


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--artifact-dir', required=True)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    root = Path.cwd().resolve()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    report = {'schema': 'native-routine-txn-source31-v1', 'success': False, 'cells_expected': 12,
              'scope': 'native PG simple and extended semantic baseline; no Rust/rawKV/sequence or performance acceptance', 'cells': []}
    try:
        before = source_check(root)
        write_json(output / 'source-before.json', before)
        binary = artifact_check(root, Path(args.artifact_dir).resolve(), output, before)
        for history in (False, True):
            for family in ('simple', 'extended'):
                for kind in ('read', 'write', 'call'):
                    report['cells'].append(run_cell(binary, output, history, family, kind))
                    write_json(output / 'result.json', report)
        after = source_check(root)
        write_json(output / 'source-after.json', after)
        if before != after or sha(binary.read_bytes()) != BINARY_SHA:
            raise ValueError('source/native identity changed during runtime')
        report['source_guards_pass'] = True
        report['cells_attempted'] = len(report['cells'])
        report['cells_passed'] = sum(c['status'] == 'passed' for c in report['cells'])
        report['success'] = len(report['cells']) == 12 and report['cells_passed'] == 12 and all(c.get('cleanup', {}).get('owned') for c in report['cells'])
    except BaseException as error:
        report['exception'] = {'type': type(error).__name__, 'message': str(error)}
        if isinstance(error, (KeyboardInterrupt, SystemExit)):
            raise
    finally:
        write_json(output / 'result.json', report)
    print('NATIVE_ROUTINE_RESULT ' + json.dumps({k: report.get(k) for k in ('success', 'cells_attempted', 'cells_passed')}), flush=True)
    return 0 if report['success'] else 1


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt('SIGTERM')))
    raise SystemExit(main())
