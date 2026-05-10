import { formatDistanceToNow, formatDistanceToNowStrict } from 'date-fns';

export function relativeTime(value?: string | null) {
  if (!value) {
    return 'unknown';
  }

  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return 'unknown';
  }

  return `${formatDistanceToNow(date, { addSuffix: true })}`;
}

export function compactRelativeTime(value?: string | null) {
  if (!value) {
    return '';
  }

  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return '';
  }

  return formatDistanceToNowStrict(date, { addSuffix: true });
}

export function greetingFor(date = new Date()) {
  const hour = date.getHours();
  if (hour < 12) {
    return 'Morning';
  }
  if (hour < 18) {
    return 'Afternoon';
  }
  return 'Evening';
}

export function firstName(name?: string | null) {
  const trimmed = name?.trim();
  if (!trimmed) {
    return 'there';
  }
  return trimmed.split(/\s+/)[0] ?? trimmed;
}
