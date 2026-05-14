import { requestJson, toQuery } from '@/lib/api/request';
import type { SearchResponse } from '@/lib/api/types';

export function searchWorkspace(query: string) {
  return requestJson<SearchResponse>(`/api/search${toQuery({ q: query })}`);
}
