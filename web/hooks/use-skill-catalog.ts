'use client';

import { useCallback, useState } from 'react';
import { listSkills } from '@/lib/api/skills';
import type { SkillSummary } from '@/lib/api/types';

type UseSkillCatalogOptions = {
  pageSize?: number;
  maxItems?: number;
};

function mergeByName(current: readonly SkillSummary[], incoming: readonly SkillSummary[]) {
  const byName = new Map(current.map((skill) => [skill.name, skill]));
  for (const skill of incoming) {
    byName.set(skill.name, skill);
  }
  return [...byName.values()].sort((left, right) => left.name.localeCompare(right.name));
}

export function useSkillCatalog({
  pageSize = 100,
  maxItems = 5_000,
}: UseSkillCatalogOptions = {}) {
  const [items, setItems] = useState<SkillSummary[]>([]);
  const [nextOffset, setNextOffset] = useState<number | null>(0);
  const [loading, setLoading] = useState(false);
  const [loadedInitial, setLoadedInitial] = useState(false);
  const [loadedAll, setLoadedAll] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadPage = useCallback(async (offset: number, replace = false) => {
    setLoading(true);
    setError(null);
    try {
      const response = await listSkills({ limit: pageSize, offset });
      setItems((current) => replace ? mergeByName([], response.items) : mergeByName(current, response.items));
      setNextOffset(response.nextOffset);
      setLoadedInitial(true);
      if (response.nextOffset === null) {
        setLoadedAll(true);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load skills.');
    } finally {
      setLoading(false);
    }
  }, [pageSize]);

  const loadInitial = useCallback(async () => {
    if (loadedInitial || loading) {
      return;
    }
    await loadPage(0, true);
  }, [loadPage, loadedInitial, loading]);

  const loadNextPage = useCallback(async () => {
    if (nextOffset === null || loading) {
      return;
    }
    await loadPage(nextOffset);
  }, [loadPage, loading, nextOffset]);

  const loadAll = useCallback(async () => {
    if (loadedAll || loading) {
      return;
    }

    setLoading(true);
    setError(null);
    try {
      const byName = new Map(items.map((skill) => [skill.name, skill]));
      let offset = loadedInitial ? nextOffset : 0;

      while (offset !== null && offset < maxItems) {
        const response = await listSkills({ limit: pageSize, offset });
        for (const skill of response.items) {
          byName.set(skill.name, skill);
        }
        if (response.nextOffset === null || response.items.length === 0) {
          offset = null;
          break;
        }
        offset = response.nextOffset;
      }

      setItems([...byName.values()].sort((left, right) => left.name.localeCompare(right.name)));
      setNextOffset(offset);
      setLoadedInitial(true);
      setLoadedAll(offset === null || offset >= maxItems);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load skills.');
    } finally {
      setLoading(false);
    }
  }, [items, loadedAll, loadedInitial, loading, maxItems, nextOffset, pageSize]);

  return {
    items,
    nextOffset,
    loading,
    error,
    loadedInitial,
    loadedAll,
    loadInitial,
    loadNextPage,
    loadAll,
  };
}
