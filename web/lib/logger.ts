/**
 * Single-line JSON logs for the admin dashboard (browser console + server components).
 * Avoid putting secrets or tokens in `context`.
 */

export type LogLevel = 'debug' | 'info' | 'warn' | 'error';

export interface LogRecord {
  timestamp: string;
  level: LogLevel;
  message: string;
  context?: Record<string, unknown>;
}

function emit(record: LogRecord): void {
  const line = JSON.stringify(record);
  switch (record.level) {
    case 'debug':
      // eslint-disable-next-line no-console -- intentional structured logging
      console.debug(line);
      break;
    case 'info':
      // eslint-disable-next-line no-console -- intentional structured logging
      console.info(line);
      break;
    case 'warn':
      // eslint-disable-next-line no-console -- intentional structured logging
      console.warn(line);
      break;
    case 'error':
      // eslint-disable-next-line no-console -- intentional structured logging
      console.error(line);
      break;
    default: {
      const _exhaustive: never = record.level;
      return _exhaustive;
    }
  }
}

function base(
  level: LogLevel,
  message: string,
  context?: Record<string, unknown>,
): void {
  emit({
    timestamp: new Date().toISOString(),
    level,
    message,
    ...(context && Object.keys(context).length > 0 ? { context } : {}),
  });
}

export const logger = {
  debug(message: string, context?: Record<string, unknown>): void {
    base('debug', message, context);
  },
  info(message: string, context?: Record<string, unknown>): void {
    base('info', message, context);
  },
  warn(message: string, context?: Record<string, unknown>): void {
    base('warn', message, context);
  },
  error(message: string, context?: Record<string, unknown>): void {
    base('error', message, context);
  },
};
