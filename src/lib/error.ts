/** Error kinds matching the Rust `AppError` enum. */
export const ERROR_KINDS = [
  "database",
  "io",
  "network",
  "git",
  "not_found",
  "invalid_input",
  "cancelled",
  "internal",
] as const;

export type ErrorKind = (typeof ERROR_KINDS)[number];

export const AI_ERROR_KINDS = [
  "validation",
  "configuration",
  "security",
  "provider",
  "state",
  "storage",
  "internal",
] as const;

export const AI_ERROR_CODES = [
  "invalid_target",
  "no_document",
  "unsafe_path",
  "not_configured",
  "key_unavailable",
  "invalid_config",
  "invalid_base_url",
  "content_changed",
  "http_auth",
  "http_request",
  "rate_limited",
  "provider_response",
  "invalid_json",
  "schema_validation",
  "cancelled",
  "conflict",
  "duplicate_target",
  "ambiguous_target",
  "unreadable_document",
  "invalid_utf8",
  "document_too_large",
  "preview_not_found",
  "preview_expired",
  "preview_consumed",
  "http_timeout",
  "invalid_state",
  "not_found",
  "response_too_large",
  "db",
  "keyring",
  "internal",
] as const;

export type AiErrorKind = (typeof AI_ERROR_KINDS)[number];
export type AiErrorCode = (typeof AI_ERROR_CODES)[number];

/** Structured error returned by Tauri commands. */
export interface AppError {
  kind: ErrorKind;
  message: string;
}

/** Structured error returned only by AI commands. */
export interface AiCommandError {
  kind: AiErrorKind;
  code: AiErrorCode;
  message: string;
  retryable: boolean;
  next_retry_at: number | null;
}

const validKinds: ReadonlySet<string> = new Set(ERROR_KINDS);
const validAiKinds: ReadonlySet<string> = new Set(AI_ERROR_KINDS);
const validAiCodes: ReadonlySet<string> = new Set(AI_ERROR_CODES);

/** Type-guard: check if an unknown error is a structured `AppError`. */
export function isAppError(error: unknown): error is AppError {
  if (
    typeof error !== "object" ||
    error === null ||
    typeof (error as AppError).message !== "string"
  ) {
    return false;
  }
  return validKinds.has((error as AppError).kind);
}

/** AI errors carry stable recovery metadata; UI decisions must not parse messages. */
export function isAiCommandError(error: unknown): error is AiCommandError {
  if (typeof error !== "object" || error === null) return false;
  const candidate = error as Partial<AiCommandError>;
  return (
    typeof candidate.kind === "string" &&
    validAiKinds.has(candidate.kind) &&
    typeof candidate.code === "string" &&
    validAiCodes.has(candidate.code) &&
    typeof candidate.message === "string" &&
    typeof candidate.retryable === "boolean" &&
    (candidate.next_retry_at === null || typeof candidate.next_retry_at === "number")
  );
}

/**
 * Extract a human-readable message from any error shape.
 * Handles structured `AppError`, plain strings, and `Error` instances.
 */
export function getErrorMessage(error: unknown, fallback: string): string {
  if (isAppError(error)) return error.message;
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === "string" && error) return error;
  return fallback;
}

/** Extract the error kind (or `undefined` for non-structured errors). */
export function getErrorKind(error: unknown): ErrorKind | undefined {
  if (isAppError(error)) return error.kind;
  return undefined;
}
