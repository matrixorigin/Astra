import { requestJson } from '@/lib/api/request';
import type { SidebarData } from '@/lib/api/types';

export function getSidebarData() {
  return requestJson<SidebarData>('/api/sidebar');
}
