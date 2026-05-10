import { NextResponse } from 'next/server';
import { getSidebar } from '@/lib/api/web-store';

export async function GET() {
  return NextResponse.json(getSidebar());
}
