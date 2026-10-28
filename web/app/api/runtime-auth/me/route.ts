import { NextResponse } from 'next/server';
import { getCurrentUser } from '@/lib/auth/actions';
import { authErrorResponse } from '@/lib/auth/http-error';

export async function GET() {
  try {
    const user = await getCurrentUser();
    return user ? NextResponse.json(user) : NextResponse.json({ error: 'Not authenticated' }, { status: 401 });
  } catch (error) { return authErrorResponse(error); }
}
