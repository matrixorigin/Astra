'use client';

import { useCallback, useState } from 'react';
import { listSkills } from '@/lib/api/skills';
import type { SkillSummary } from '@/lib/api/types';
import type { RuntimeSkillListCursor } from '@astra/sdk';

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
  const [nextCursor, setNextCursor] = useState<RuntimeSkillListCursor | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadedInitial, setLoadedInitial] = useState(false);
  const [loadedAll, setLoadedAll] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadPage = useCallback(async (cursor: RuntimeSkillListCursor | null, replace = false) => {
    setLoading(true);
    setError(null);
    try {
      const response = await listSkills({ limit: pageSize, cursor });
      setItems((current) => replace ? mergeByName([], response.items) : mergeByName(current, response.items));
      setNextCursor(response.nextCursor);
      setLoadedInitial(true);
      if (response.nextCursor === null) {
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
    await loadPage(null, true);
  }, [loadPage, loadedInitial, loading]);

  const loadNextPage = useCallback(async () => {
    if (nextCursor === null || loading) {
      return;
    }
    await loadPage(nextCursor);
  }, [loadPage, loading, nextCursor]);

  const loadAll = useCallback(async () => {
    if (loadedAll || loading) {
      return;
    }

    setLoading(true);
    setError(null);
    try {
      const byName = new Map(items.map((skill) => [skill.name, skill]));
      let cursor = loadedInitial ? nextCursor : null;
      let loadedCount = byName.size;

      while (loadedCount < maxItems) {
        const response = await listSkills({ limit: pageSize, cursor });
        for (const skill of response.items) {
          byName.set(skill.name, skill);
        }
        loadedCount = byName.size;
        if (response.nextCursor === null || response.items.length === 0) {
          cursor = null;
          break;
        }
        cursor = response.nextCursor;
      }

      setItems([...byName.values()].sort((left, right) => left.name.localeCompare(right.name)));
      setNextCursor(cursor);
      setLoadedInitial(true);
      setLoadedAll(cursor === null || loadedCount >= maxItems);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load skills.');
    } finally {
      setLoading(false);
    }
  }, [items, loadedAll, loadedInitial, loading, maxItems, nextCursor, pageSize]);

  return {
    items,
    nextCursor,
    loading,
    error,
    loadedInitial,
    loadedAll,
    loadInitial,
    loadNextPage,
    loadAll,
  };
}
