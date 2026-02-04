/**
 * HeliosDB error classes
 */

/**
 * Base error class for all HeliosDB errors
 */
export class HeliosDBError extends Error {
  readonly code?: string;
  readonly details?: Record<string, unknown>;

  constructor(message: string, code?: string, details?: Record<string, unknown>) {
    super(message);
    this.name = 'HeliosDBError';
    this.code = code;
    this.details = details;

    // Maintains proper stack trace for where error was thrown
    if (Error.captureStackTrace) {
      Error.captureStackTrace(this, this.constructor);
    }
  }

  toString(): string {
    return this.code ? `[${this.code}] ${this.message}` : this.message;
  }
}

/**
 * Connection error
 */
export class ConnectionError extends HeliosDBError {
  constructor(message: string, details?: Record<string, unknown>) {
    super(message, 'CONNECTION_ERROR', details);
    this.name = 'ConnectionError';
  }
}

/**
 * Query execution error
 */
export class QueryError extends HeliosDBError {
  readonly sql?: string;

  constructor(message: string, sql?: string, details?: Record<string, unknown>) {
    super(message, 'QUERY_ERROR', details);
    this.name = 'QueryError';
    this.sql = sql;
  }
}

/**
 * Authentication error
 */
export class AuthenticationError extends HeliosDBError {
  constructor(message: string, details?: Record<string, unknown>) {
    super(message, 'AUTHENTICATION_ERROR', details);
    this.name = 'AuthenticationError';
  }
}

/**
 * Resource not found error
 */
export class NotFoundError extends HeliosDBError {
  readonly resourceType?: string;
  readonly resourceId?: string;

  constructor(
    message: string,
    resourceType?: string,
    resourceId?: string,
    details?: Record<string, unknown>
  ) {
    super(message, 'NOT_FOUND', details);
    this.name = 'NotFoundError';
    this.resourceType = resourceType;
    this.resourceId = resourceId;
  }
}

/**
 * Conflict error
 */
export class ConflictError extends HeliosDBError {
  constructor(message: string, details?: Record<string, unknown>) {
    super(message, 'CONFLICT', details);
    this.name = 'ConflictError';
  }
}

/**
 * Validation error
 */
export class ValidationError extends HeliosDBError {
  readonly field?: string;

  constructor(message: string, field?: string, details?: Record<string, unknown>) {
    super(message, 'VALIDATION_ERROR', details);
    this.name = 'ValidationError';
    this.field = field;
  }
}

/**
 * Timeout error
 */
export class TimeoutError extends HeliosDBError {
  readonly timeoutMs?: number;

  constructor(message: string, timeoutMs?: number, details?: Record<string, unknown>) {
    super(message, 'TIMEOUT', details);
    this.name = 'TimeoutError';
    this.timeoutMs = timeoutMs;
  }
}

/**
 * Rate limit error
 */
export class RateLimitError extends HeliosDBError {
  readonly retryAfter?: number;

  constructor(message: string, retryAfter?: number, details?: Record<string, unknown>) {
    super(message, 'RATE_LIMIT_EXCEEDED', details);
    this.name = 'RateLimitError';
    this.retryAfter = retryAfter;
  }
}

/**
 * Create appropriate error from HTTP response
 */
export function createErrorFromResponse(
  status: number,
  data: Record<string, unknown>
): HeliosDBError {
  const message = (data.message as string) || `HTTP ${status}`;
  const details = data.details as Record<string, unknown> | undefined;

  switch (status) {
    case 400:
      return new ValidationError(message, undefined, details);
    case 401:
      return new AuthenticationError(message, details);
    case 404:
      return new NotFoundError(message, undefined, undefined, details);
    case 409:
      return new ConflictError(message, details);
    case 429:
      return new RateLimitError(message, undefined, details);
    default:
      return new HeliosDBError(message, data.code as string | undefined, details);
  }
}
