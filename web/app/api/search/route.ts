import { NextRequest, NextResponse } from 'next/server';
import { searchData } from '@/lib/api/web-store';

export async function GET(request: NextRequest) {
  return NextResponse.json(searchData(request.nextUrl.searchParams.get('q') ?? ''));
}
