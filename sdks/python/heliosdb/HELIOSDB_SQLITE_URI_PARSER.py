"""
HeliosDB SQLite URI Parser

RFC 3986 compliant URI parser for SQLite and HeliosDB connection strings.

Supports:
- sqlite:// URIs (standard SQLite)
- heliosdb:// URIs (explicit HeliosDB mode)
- file:// URIs (file paths)
- In-memory databases (:memory:)
- All SQLite URI parameters
- Mode flags and options

Examples:
    sqlite:///path/to/database.db
    sqlite:///:memory:
    sqlite:///mydb.db?mode=ro&cache=shared
    heliosdb://localhost:8080/mydb
    heliosdb:///path/to/database.db?mode=daemon&port=6543
    heliosdb:///:memory:?mode=repl
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Dict, Optional, Any
from urllib.parse import parse_qs, unquote, urlparse, ParseResult


class URIScheme(str, Enum):
    """Supported URI schemes."""

    SQLITE = "sqlite"
    HELIOSDB = "heliosdb"
    FILE = "file"


class HeliosDBMode(str, Enum):
    """HeliosDB operational modes."""

    REPL = "repl"          # Direct embedded mode (REPL-style)
    SERVER = "server"      # REST API client mode
    DAEMON = "daemon"      # Background daemon mode
    AUTO = "auto"          # Auto-detect based on URI


class SQLiteOpenMode(str, Enum):
    """SQLite database open modes."""

    READ_WRITE_CREATE = "rwc"  # Read-write, create if missing (default)
    READ_WRITE = "rw"          # Read-write, must exist
    READ_ONLY = "ro"           # Read-only
    MEMORY = "memory"          # In-memory database


class CacheMode(str, Enum):
    """SQLite cache sharing modes."""

    SHARED = "shared"      # Shared cache between connections
    PRIVATE = "private"    # Private cache per connection (default)


@dataclass
class ParsedURI:
    """
    Parsed and validated database URI.

    Attributes:
        scheme: URI scheme (sqlite, heliosdb, file)
        path: Database file path or :memory:
        host: Server host (for server mode)
        port: Server port (for server mode)
        mode: HeliosDB operational mode
        sqlite_mode: SQLite open mode
        cache_mode: SQLite cache mode
        is_memory: Whether this is an in-memory database
        is_remote: Whether this connects to a remote server
        parameters: Additional URI parameters
        raw_uri: Original URI string
    """

    scheme: URIScheme
    path: Optional[str] = None
    host: Optional[str] = None
    port: Optional[int] = None
    mode: HeliosDBMode = HeliosDBMode.AUTO
    sqlite_mode: SQLiteOpenMode = SQLiteOpenMode.READ_WRITE_CREATE
    cache_mode: CacheMode = CacheMode.PRIVATE
    is_memory: bool = False
    is_remote: bool = False
    parameters: Dict[str, Any] = field(default_factory=dict)
    raw_uri: str = ""

    @property
    def is_embedded(self) -> bool:
        """Check if this is an embedded (local file) database."""
        return not self.is_remote and not self.is_memory

    @property
    def effective_mode(self) -> HeliosDBMode:
        """Get the effective mode, resolving AUTO."""
        if self.mode != HeliosDBMode.AUTO:
            return self.mode

        # Auto-detect mode
        if self.is_remote:
            return HeliosDBMode.SERVER
        elif self.is_memory or self.is_embedded:
            return HeliosDBMode.REPL
        else:
            return HeliosDBMode.REPL

    @property
    def connection_string(self) -> str:
        """Build connection string for the database backend."""
        if self.is_memory:
            return ":memory:"
        elif self.is_remote:
            protocol = "https" if self.port == 443 else "http"
            port_str = f":{self.port}" if self.port else ""
            return f"{protocol}://{self.host}{port_str}"
        else:
            return str(self.path)

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary representation."""
        return {
            "scheme": self.scheme.value,
            "path": self.path,
            "host": self.host,
            "port": self.port,
            "mode": self.mode.value,
            "sqlite_mode": self.sqlite_mode.value,
            "cache_mode": self.cache_mode.value,
            "is_memory": self.is_memory,
            "is_remote": self.is_remote,
            "parameters": self.parameters,
            "effective_mode": self.effective_mode.value,
            "connection_string": self.connection_string,
        }


class URIParser:
    """
    RFC 3986 compliant URI parser for HeliosDB connection strings.

    Parses and validates SQLite and HeliosDB URIs, extracting all
    components and parameters.
    """

    # URI validation patterns
    MEMORY_PATTERN = re.compile(r'^:memory:$', re.IGNORECASE)
    PORT_PATTERN = re.compile(r'^\d{1,5}$')

    @classmethod
    def parse(cls, uri: str, expand_env: bool = True) -> ParsedURI:
        """
        Parse a database URI into components.

        Args:
            uri: Database URI string
            expand_env: Whether to expand environment variables

        Returns:
            ParsedURI object with all extracted components

        Raises:
            ValueError: If URI is invalid or malformed

        Examples:
            >>> parser = URIParser()
            >>> result = parser.parse("sqlite:///mydb.db")
            >>> result.path
            '/mydb.db'

            >>> result = parser.parse("heliosdb://localhost:8080/api/v1")
            >>> result.host
            'localhost'
        """
        if not uri:
            raise ValueError("URI cannot be empty")

        # Expand environment variables if requested
        if expand_env:
            uri = os.path.expandvars(uri)
            uri = os.path.expanduser(uri)

        # Parse the URI
        try:
            parsed = urlparse(uri)
        except Exception as e:
            raise ValueError(f"Invalid URI format: {e}") from e

        # Validate scheme
        scheme = cls._parse_scheme(parsed)

        # Parse based on scheme
        if parsed.netloc:
            # Remote server URI (host:port/path)
            result = cls._parse_remote_uri(parsed, scheme, uri)
        else:
            # Local file or memory URI
            result = cls._parse_local_uri(parsed, scheme, uri)

        # Parse query parameters
        cls._parse_parameters(parsed, result)

        # Validate the parsed result
        cls._validate_parsed_uri(result)

        return result

    @classmethod
    def _parse_scheme(cls, parsed: ParseResult) -> URIScheme:
        """Extract and validate URI scheme."""
        if not parsed.scheme:
            raise ValueError("URI must include a scheme (sqlite:// or heliosdb://)")

        scheme_lower = parsed.scheme.lower()

        try:
            return URIScheme(scheme_lower)
        except ValueError:
            raise ValueError(
                f"Unsupported URI scheme: {parsed.scheme}. "
                f"Supported schemes: {', '.join(s.value for s in URIScheme)}"
            ) from None

    @classmethod
    def _parse_remote_uri(
        cls,
        parsed: ParseResult,
        scheme: URIScheme,
        raw_uri: str
    ) -> ParsedURI:
        """Parse remote server URI."""
        host = parsed.hostname
        port = parsed.port

        if not host:
            raise ValueError("Remote URI must specify a host")

        # Default ports
        if port is None:
            port = 8080 if scheme == URIScheme.HELIOSDB else 5432

        # Validate port
        if not (1 <= port <= 65535):
            raise ValueError(f"Invalid port number: {port}")

        return ParsedURI(
            scheme=scheme,
            host=host,
            port=port,
            is_remote=True,
            raw_uri=raw_uri,
        )

    @classmethod
    def _parse_local_uri(
        cls,
        parsed: ParseResult,
        scheme: URIScheme,
        raw_uri: str
    ) -> ParsedURI:
        """Parse local file or memory URI."""
        path = parsed.path

        # Decode URL encoding
        if path:
            path = unquote(path)

        # Check for in-memory database
        is_memory = False
        if cls.MEMORY_PATTERN.match(path or ""):
            is_memory = True
            path = ":memory:"
        elif path and "memory" in path.lower():
            # Handle variations like /:memory:, //:memory:
            if ":memory:" in path:
                is_memory = True
                path = ":memory:"

        # Normalize file paths
        if not is_memory and path:
            # Remove leading slash for relative paths on Unix
            # Keep UNC paths on Windows (//server/share)
            if path.startswith("///"):
                # Absolute path: ///home/user/db.db -> /home/user/db.db
                path = path[2:]
            elif path.startswith("//") and os.name != 'nt':
                # Likely a mistake, treat as absolute
                path = path[1:]

            # Expand to absolute path
            path = str(Path(path).resolve())

        return ParsedURI(
            scheme=scheme,
            path=path,
            is_memory=is_memory,
            is_remote=False,
            raw_uri=raw_uri,
        )

    @classmethod
    def _parse_parameters(cls, parsed: ParseResult, result: ParsedURI) -> None:
        """Parse query parameters from URI."""
        if not parsed.query:
            return

        params = parse_qs(parsed.query, keep_blank_values=True)

        # Extract known parameters
        for key, values in params.items():
            value = values[0] if values else None

            if key == "mode":
                # HeliosDB mode or SQLite mode
                if value in ("repl", "server", "daemon", "auto"):
                    result.mode = HeliosDBMode(value)
                elif value in ("ro", "rw", "rwc", "memory"):
                    result.sqlite_mode = SQLiteOpenMode(value)
                else:
                    result.parameters[key] = value

            elif key == "cache":
                if value in ("shared", "private"):
                    result.cache_mode = CacheMode(value)
                else:
                    result.parameters[key] = value

            elif key == "port":
                # Daemon port override
                try:
                    result.port = int(value)
                except (ValueError, TypeError):
                    raise ValueError(f"Invalid port parameter: {value}") from None

            elif key == "vfs":
                # Virtual file system
                result.parameters["vfs"] = value

            elif key in ("timeout", "busy_timeout"):
                # Timeout parameters
                try:
                    result.parameters[key] = int(value)
                except (ValueError, TypeError):
                    result.parameters[key] = value

            else:
                # Store other parameters as-is
                result.parameters[key] = value

    @classmethod
    def _validate_parsed_uri(cls, result: ParsedURI) -> None:
        """Validate the parsed URI result."""
        # Remote URIs must have a host
        if result.is_remote and not result.host:
            raise ValueError("Remote URI must specify a host")

        # Local URIs must have a path (or be :memory:)
        if not result.is_remote and not result.is_memory and not result.path:
            raise ValueError("Local URI must specify a database path")

        # Memory databases can't be remote
        if result.is_memory and result.is_remote:
            raise ValueError("In-memory databases cannot be remote")

        # Validate mode compatibility
        if result.mode == HeliosDBMode.SERVER and not result.is_remote:
            raise ValueError("Server mode requires a remote URI (host:port)")


def parse_uri(uri: str, expand_env: bool = True) -> ParsedURI:
    """
    Convenience function to parse a database URI.

    Args:
        uri: Database URI string
        expand_env: Whether to expand environment variables

    Returns:
        ParsedURI object

    Examples:
        >>> from heliosdb.uri_parser import parse_uri
        >>> result = parse_uri("sqlite:///mydb.db")
        >>> print(result.path)
        /mydb.db
    """
    return URIParser.parse(uri, expand_env=expand_env)
