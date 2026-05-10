import { clsx, type ClassValue } from 'clsx';

export function cn(...values: ClassValue[]) {
  return clsx(values);
}
