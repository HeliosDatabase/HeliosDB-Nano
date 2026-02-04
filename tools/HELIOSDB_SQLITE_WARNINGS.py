"""
HeliosDB SQLite Warnings System
================================

Provides custom warning classes and error handling for HeliosDB-Lite fallback mechanisms.
Integrates with Python's warnings system and provides structured logging.

This module defines:
- Custom warning classes for different fallback scenarios
- Structured error messages with actionable guidance
- Logging integration with contextual information
- Developer-friendly vs. silent handling options

Author: HeliosDB Team
License: Apache-2.0
Version: 3.0.1
"""

import warnings
import logging
import sys
from typing import Optional, Dict, Any, List
from enum import Enum
from dataclasses import dataclass, field
from datetime import datetime


class WarningSeverity(Enum):
    """Severity levels for warnings"""
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    CRITICAL = "critical"


class HeliosDBFeatureWarning(UserWarning):
    """Base warning class for HeliosDB feature fallbacks"""
    pass


class FeatureNotSupportedWarning(HeliosDBFeatureWarning):
    """Warning for completely unsupported features"""
    pass


class PrecisionLossWarning(HeliosDBFeatureWarning):
    """Warning for type conversions that may lose precision"""
    pass


class FallbackStrategyWarning(HeliosDBFeatureWarning):
    """Warning for features requiring application-level implementation"""
    pass


class PerformanceImpactWarning(HeliosDBFeatureWarning):
    """Warning for fallbacks with performance implications"""
    pass


class SecurityConsiderationWarning(HeliosDBFeatureWarning):
    """Warning for fallbacks with security implications"""
    pass


class DeprecationWarning(HeliosDBFeatureWarning):
    """Warning for deprecated fallback strategies"""
    pass


@dataclass
class FallbackWarning:
    """
    Structured warning with metadata for fallback operations.

    Provides rich context about why a fallback was needed and what
    actions the developer should take.
    """
    feature: str
    severity: WarningSeverity
    message: str
    guidance: List[str] = field(default_factory=list)
    mitigation: Optional[str] = None
    documentation_url: Optional[str] = None
    timestamp: datetime = field(default_factory=datetime.utcnow)
    additional_context: Dict[str, Any] = field(default_factory=dict)

    def format_message(self, include_guidance: bool = True) -> str:
        """Format warning message with optional guidance"""
        lines = [
            f"[{self.severity.value.upper()}] {self.feature}: {self.message}"
        ]

        if include_guidance and self.guidance:
            lines.append("\nGuidance:")
            for item in self.guidance:
                lines.append(f"  - {item}")

        if self.mitigation:
            lines.append(f"\nMitigation: {self.mitigation}")

        if self.documentation_url:
            lines.append(f"\nDocumentation: {self.documentation_url}")

        return "\n".join(lines)

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary for logging"""
        return {
            "feature": self.feature,
            "severity": self.severity.value,
            "message": self.message,
            "guidance": self.guidance,
            "mitigation": self.mitigation,
            "documentation_url": self.documentation_url,
            "timestamp": self.timestamp.isoformat(),
            "additional_context": self.additional_context
        }


class FallbackLogger:
    """
    Structured logger for fallback operations.

    Provides consistent logging format with rich contextual information
    and integrates with Python's logging system.
    """

    _loggers: Dict[str, logging.Logger] = {}

    @classmethod
    def get_logger(
        cls,
        name: str = "heliosdb.fallback",
        level: int = logging.WARNING
    ) -> logging.Logger:
        """
        Get or create a logger instance.

        Args:
            name: Logger name
            level: Logging level (default: WARNING)

        Returns:
            Configured logger instance
        """
        if name not in cls._loggers:
            logger = logging.getLogger(name)
            logger.setLevel(level)

            # Add console handler if not already present
            if not logger.handlers:
                handler = logging.StreamHandler(sys.stdout)
                handler.setLevel(level)

                # Use structured format
                formatter = logging.Formatter(
                    fmt='%(asctime)s - %(name)s - %(levelname)s - %(message)s',
                    datefmt='%Y-%m-%d %H:%M:%S'
                )
                handler.setFormatter(formatter)
                logger.addHandler(handler)

            cls._loggers[name] = logger

        return cls._loggers[name]

    @classmethod
    def log_fallback(
        cls,
        warning: FallbackWarning,
        logger: Optional[logging.Logger] = None
    ):
        """
        Log a fallback warning with structured data.

        Args:
            warning: FallbackWarning instance
            logger: Custom logger (uses default if None)
        """
        log = logger or cls.get_logger()

        # Map severity to log level
        level_map = {
            WarningSeverity.INFO: logging.INFO,
            WarningSeverity.WARNING: logging.WARNING,
            WarningSeverity.ERROR: logging.ERROR,
            WarningSeverity.CRITICAL: logging.CRITICAL
        }
        level = level_map.get(warning.severity, logging.WARNING)

        # Log with extra context
        log.log(
            level,
            warning.format_message(include_guidance=True),
            extra={
                "feature": warning.feature,
                "guidance": warning.guidance,
                "additional_context": warning.additional_context
            }
        )


class FallbackErrorHandler:
    """
    Handles errors with helpful messages and mitigation strategies.

    Provides developer-friendly error messages that explain the problem
    and suggest concrete solutions.
    """

    # Error templates with guidance
    ERROR_TEMPLATES = {
        "DECIMAL_PRECISION_LOSS": FallbackWarning(
            feature="DECIMAL/NUMERIC",
            severity=WarningSeverity.WARNING,
            message="Type conversion may result in precision loss",
            guidance=[
                "DECIMAL/NUMERIC types converted to FLOAT8 (64-bit floating point)",
                "Financial calculations may accumulate rounding errors",
                "Consider storing monetary values as integer cents (e.g., $12.34 → 1234 cents)",
                "Use INT8 for amounts up to 92,233,720,368,547,758.07 ($92 quadrillion)"
            ],
            mitigation="Store monetary values as integer cents/pennies using INT8",
            documentation_url="https://heliosdb.dev/docs/types#decimal-fallback"
        ),
        "TRIGGER_NOT_SUPPORTED": FallbackWarning(
            feature="TRIGGER",
            severity=WarningSeverity.ERROR,
            message="Database triggers are not supported in HeliosDB-Lite",
            guidance=[
                "Implement trigger logic in application code",
                "Use ORM pre/post save hooks (e.g., Django signals, SQLAlchemy events)",
                "Implement validation middleware",
                "Add application-level event listeners",
                "Consider using database branching for testing trigger logic"
            ],
            mitigation="Move trigger logic to application layer with proper testing",
            documentation_url="https://heliosdb.dev/docs/features#trigger-alternative"
        ),
        "CHECK_CONSTRAINT_FALLBACK": FallbackWarning(
            feature="CHECK Constraint",
            severity=WarningSeverity.WARNING,
            message="CHECK constraints require application-level validation",
            guidance=[
                "Implement validation before INSERT/UPDATE operations",
                "Use ORM validators (e.g., Pydantic, Marshmallow)",
                "Add validation at API entry points",
                "Consider using database branching to test validation logic",
                "Document constraints in application code and database comments"
            ],
            mitigation="Implement validation in application layer with comprehensive testing",
            documentation_url="https://heliosdb.dev/docs/features#check-constraint"
        ),
        "FOREIGN_KEY_FALLBACK": FallbackWarning(
            feature="Foreign Key",
            severity=WarningSeverity.WARNING,
            message="Foreign key constraints require application-level enforcement",
            guidance=[
                "Validate foreign key references before INSERT/UPDATE",
                "Use transactions to ensure referential integrity",
                "Implement CASCADE DELETE logic in application",
                "Add indexes on foreign key columns for performance",
                "Consider using database branching for testing referential integrity",
                "Document relationships in application code and schema comments"
            ],
            mitigation="Enforce referential integrity in application with transactional writes",
            documentation_url="https://heliosdb.dev/docs/features#foreign-key"
        ),
        "AUTOINCREMENT_ALTERNATIVE": FallbackWarning(
            feature="AUTOINCREMENT",
            severity=WarningSeverity.INFO,
            message="Consider using UUID for primary keys",
            guidance=[
                "UUID provides guaranteed uniqueness across distributed systems",
                "No sequence management or coordination needed",
                "UUID4: Random, cryptographically secure",
                "UUID7: Time-ordered, good for indexing performance",
                "Alternative: Use SERIAL (INT4) for simple auto-increment",
                "Alternative: Implement manual sequence with MAX(id) + 1 (requires locking)"
            ],
            mitigation="Use UUID type for primary keys",
            documentation_url="https://heliosdb.dev/docs/types#uuid-vs-serial"
        ),
    }

    @classmethod
    def get_error_guidance(cls, error_type: str) -> Optional[FallbackWarning]:
        """Get structured error guidance for an error type"""
        return cls.ERROR_TEMPLATES.get(error_type)

    @classmethod
    def raise_with_guidance(cls, error_type: str, custom_message: Optional[str] = None):
        """
        Raise a ValueError with helpful guidance.

        Args:
            error_type: Type of error (key in ERROR_TEMPLATES)
            custom_message: Optional custom message to prepend

        Raises:
            ValueError: With formatted error message and guidance
        """
        warning = cls.ERROR_TEMPLATES.get(error_type)
        if not warning:
            raise ValueError(f"Unknown error type: {error_type}")

        message_parts = []
        if custom_message:
            message_parts.append(custom_message)
        message_parts.append(warning.format_message(include_guidance=True))

        raise ValueError("\n".join(message_parts))


class WarningManager:
    """
    Manages warning display and filtering.

    Provides control over which warnings are shown and how they're formatted.
    """

    def __init__(
        self,
        show_warnings: bool = True,
        show_guidance: bool = True,
        show_once_per_feature: bool = True
    ):
        """
        Initialize warning manager.

        Args:
            show_warnings: If False, suppress all warnings
            show_guidance: If True, include guidance in warning messages
            show_once_per_feature: If True, show each feature warning only once per session
        """
        self.show_warnings = show_warnings
        self.show_guidance = show_guidance
        self.show_once_per_feature = show_once_per_feature

        # Track which features have been warned about
        self._warned_features: set = set()

    def emit_warning(
        self,
        warning: FallbackWarning,
        warning_class: type = HeliosDBFeatureWarning,
        stacklevel: int = 2
    ):
        """
        Emit a warning with optional deduplication.

        Args:
            warning: FallbackWarning to emit
            warning_class: Warning class to use
            stacklevel: Stack level for warning (default: 2)
        """
        if not self.show_warnings:
            return

        # Check if we should skip due to deduplication
        if self.show_once_per_feature:
            if warning.feature in self._warned_features:
                return
            self._warned_features.add(warning.feature)

        # Emit warning
        warnings.warn(
            warning.format_message(include_guidance=self.show_guidance),
            warning_class,
            stacklevel=stacklevel
        )

        # Also log it
        FallbackLogger.log_fallback(warning)

    def reset(self):
        """Reset warning tracking (useful for testing)"""
        self._warned_features.clear()

    def configure_warnings_filter(
        self,
        action: str = "default",
        category: type = HeliosDBFeatureWarning
    ):
        """
        Configure Python warnings filter for HeliosDB warnings.

        Args:
            action: Warning filter action ("error", "ignore", "always", "default", "module", "once")
            category: Warning category to filter
        """
        warnings.filterwarnings(action, category=category)


# Global warning manager instance
_warning_manager: Optional[WarningManager] = None


def get_warning_manager() -> WarningManager:
    """Get or create the global warning manager"""
    global _warning_manager
    if _warning_manager is None:
        _warning_manager = WarningManager()
    return _warning_manager


def configure_warning_display(
    show_warnings: bool = True,
    show_guidance: bool = True,
    show_once_per_feature: bool = True
):
    """
    Configure global warning display settings.

    Args:
        show_warnings: Show warnings or suppress them
        show_guidance: Include guidance in warning messages
        show_once_per_feature: Show each feature warning only once
    """
    global _warning_manager
    _warning_manager = WarningManager(
        show_warnings=show_warnings,
        show_guidance=show_guidance,
        show_once_per_feature=show_once_per_feature
    )


# Convenience functions for common warning scenarios
def warn_precision_loss(feature: str, details: Optional[str] = None):
    """Emit precision loss warning"""
    warning = FallbackWarning(
        feature=feature,
        severity=WarningSeverity.WARNING,
        message=f"Precision loss may occur: {details or 'Type conversion applied'}",
        guidance=FallbackErrorHandler.ERROR_TEMPLATES["DECIMAL_PRECISION_LOSS"].guidance
    )
    get_warning_manager().emit_warning(warning, PrecisionLossWarning)


def warn_feature_not_supported(feature: str, alternative: Optional[str] = None):
    """Emit feature not supported warning"""
    guidance = [f"Feature '{feature}' is not supported in HeliosDB-Lite"]
    if alternative:
        guidance.append(f"Alternative: {alternative}")

    warning = FallbackWarning(
        feature=feature,
        severity=WarningSeverity.ERROR,
        message=f"{feature} is not supported",
        guidance=guidance
    )
    get_warning_manager().emit_warning(warning, FeatureNotSupportedWarning)


def warn_application_level_required(feature: str, implementation: List[str]):
    """Emit warning for features requiring application-level implementation"""
    warning = FallbackWarning(
        feature=feature,
        severity=WarningSeverity.WARNING,
        message=f"{feature} must be implemented in application layer",
        guidance=implementation
    )
    get_warning_manager().emit_warning(warning, FallbackStrategyWarning)


def warn_performance_impact(feature: str, impact: str):
    """Emit performance impact warning"""
    warning = FallbackWarning(
        feature=feature,
        severity=WarningSeverity.INFO,
        message=f"Performance impact: {impact}",
        guidance=["Consider profiling your application to measure actual impact"]
    )
    get_warning_manager().emit_warning(warning, PerformanceImpactWarning)


def warn_security_consideration(feature: str, consideration: str):
    """Emit security consideration warning"""
    warning = FallbackWarning(
        feature=feature,
        severity=WarningSeverity.WARNING,
        message=f"Security consideration: {consideration}",
        guidance=["Review security implications before deploying to production"]
    )
    get_warning_manager().emit_warning(warning, SecurityConsiderationWarning)


# Example usage and testing
if __name__ == "__main__":
    # Configure warnings to show all
    configure_warning_display(
        show_warnings=True,
        show_guidance=True,
        show_once_per_feature=False
    )

    # Test different warning types
    print("Testing warning system...\n")

    # Precision loss warning
    warn_precision_loss("DECIMAL(10,2)", "Converting to FLOAT8")

    # Feature not supported
    warn_feature_not_supported("TRIGGER", "Implement in application code")

    # Application-level implementation
    warn_application_level_required(
        "CHECK Constraint",
        [
            "Add validation before INSERT/UPDATE",
            "Use ORM validators",
            "Add API-level validation"
        ]
    )

    # Performance warning
    warn_performance_impact("UUID Primary Keys", "Slightly larger index size")

    # Security warning
    warn_security_consideration(
        "Foreign Key Enforcement",
        "Ensure all entry points validate references"
    )

    print("\nWarning system test complete.")
