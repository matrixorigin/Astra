'use server';

import { revalidatePath } from 'next/cache';
import { resumeSession, cancelSession, closeSession } from '@/lib/api/platform';

export async function resumeSessionAction(
  sessionId: string,
): Promise<{ ok: boolean; error?: string }> {
  try {
    await resumeSession(sessionId);
    revalidatePath('/sessions');
    revalidatePath(`/sessions/${sessionId}`);
    return { ok: true };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : 'Unknown error' };
  }
}

export async function cancelSessionAction(
  sessionId: string,
): Promise<{ ok: boolean; error?: string }> {
  try {
    await cancelSession(sessionId);
    revalidatePath('/sessions');
    revalidatePath(`/sessions/${sessionId}`);
    return { ok: true };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : 'Unknown error' };
  }
}

export async function closeSessionAction(
  sessionId: string,
): Promise<{ ok: boolean; error?: string }> {
  try {
    await closeSession(sessionId);
    revalidatePath('/sessions');
    revalidatePath(`/sessions/${sessionId}`);
    return { ok: true };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : 'Unknown error' };
  }
}
