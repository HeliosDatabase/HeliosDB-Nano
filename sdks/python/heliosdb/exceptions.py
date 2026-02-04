"""
HeliosDB exceptions.

All exceptions inherit from HeliosDBError for easy catching.
"""

from typing import Any, Optional


class HeliosDBError(Exception):
    """Base exception for all HeliosDB errors."""

    def __init__(
        self,
        message: str,
        code: Optional[str] = None,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.code = code
        self.details = details or {}

    def __str__(self) -> str:
        if self.code:
            return f"[{self.code}] {self.message}"
        return self.message


class ConnectionError(HeliosDBError):
    """Raised when connection to HeliosDB fails."""

    def __init__(self, message: str, details: Optional[dict[str, Any]] = None) -> None:
        super().__init__(message, code="CONNECTION_ERROR", details=details)


class QueryError(HeliosDBError):
    """Raised when a SQL query fails."""

    def __init__(
        self,
        message: str,
        sql: Optional[str] = None,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message, code="QUERY_ERROR", details=details)
        self.sql = sql


class AuthenticationError(HeliosDBError):
    """Raised when authentication fails."""

    def __init__(self, message: str, details: Optional[dict[str, Any]] = None) -> None:
        super().__init__(message, code="AUTHENTICATION_ERROR", details=details)


class NotFoundError(HeliosDBError):
    """Raised when a requested resource is not found."""

    def __init__(
        self,
        message: str,
        resource_type: Optional[str] = None,
        resource_id: Optional[str] = None,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message, code="NOT_FOUND", details=details)
        self.resource_type = resource_type
        self.resource_id = resource_id


class ConflictError(HeliosDBError):
    """Raised when there's a conflict (e.g., merge conflict, unique constraint)."""

    def __init__(self, message: str, details: Optional[dict[str, Any]] = None) -> None:
        super().__init__(message, code="CONFLICT", details=details)


class ValidationError(HeliosDBError):
    """Raised when input validation fails."""

    def __init__(
        self,
        message: str,
        field: Optional[str] = None,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message, code="VALIDATION_ERROR", details=details)
        self.field = field


class TimeoutError(HeliosDBError):
    """Raised when an operation times out."""

    def __init__(
        self,
        message: str,
        timeout_ms: Optional[int] = None,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message, code="TIMEOUT", details=details)
        self.timeout_ms = timeout_ms


class RateLimitError(HeliosDBError):
    """Raised when rate limit is exceeded."""

    def __init__(
        self,
        message: str,
        retry_after: Optional[int] = None,
        details: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message, code="RATE_LIMIT_EXCEEDED", details=details)
        self.retry_after = retry_after
