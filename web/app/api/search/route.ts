import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { searchData } from '@/lib/api/web-store';

export async function GET(request: NextRequest) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  return NextResponse.json(searchData(auth.user.user_id, request.nextUrl.searchParams.get('q') ?? ''));
}
