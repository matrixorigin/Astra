'use client';

import { FolderPlus, Plus } from 'lucide-react';
import { useRouter, useSearchParams } from 'next/navigation';
import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { EmptyState } from '@/components/ui/empty-state';
import { PageHeader } from '@/components/ui/page-header';
import { SearchField } from '@/components/ui/search-field';
import { ProjectCard } from '@/components/app/project-card';
import { listProjects } from '@/lib/api/projects';
import type { ProjectSummary } from '@/lib/api/types';
import { useDebouncedValue } from '@/hooks/use-debounced-value';

type SortMode = 'activity' | 'created' | 'name';

export function ProjectsList() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const [query, setQuery] = useState('');
  const debounced = useDebouncedValue(query, 250);
  const [sort, setSort] = useState<SortMode>((searchParams.get('sort') as SortMode) || 'activity');
  const [items, setItems] = useState<ProjectSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const params = new URLSearchParams(searchParams.toString());
    params.set('sort', sort);
    router.replace(`/projects?${params.toString()}`, { scroll: false });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sort]);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    listProjects({ q: debounced, sort })
      .then((result) => {
        if (!cancelled) {
          setItems(result.items);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : 'Failed to load projects');
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [debounced, sort]);

  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto w-full max-w-[1200px]">
        <PageHeader title="Projects" action={<Button href="/projects/new" variant="primary" leadingIcon={Plus}>New project</Button>} />

        <SearchField
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search projects..."
          containerClassName="mt-6"
        />

        <div className="mt-6 flex items-center justify-end gap-3">
          <span className="text-sm text-text-secondary">Sort by</span>
          <select
            value={sort}
            onChange={(event) => setSort(event.target.value as SortMode)}
            className="h-9 rounded-control border border-border bg-surface px-3 text-sm outline-none focus:border-accent"
          >
            <option value="activity">Activity</option>
            <option value="created">Created</option>
            <option value="name">Name</option>
          </select>
        </div>

        {error ? (
          <div className="mt-6 rounded-card border border-danger/20 bg-danger/5 px-4 py-3 text-sm text-danger">
            {error}
          </div>
        ) : null}

        <div className="mt-6 grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
          {loading
            ? Array.from({ length: 6 }).map((_, index) => (
                <div key={index} className="h-[140px] rounded-card border border-border bg-surface" />
              ))
            : items.map((project) => <ProjectCard key={project.id} project={project} />)}
        </div>

        {!loading && items.length === 0 ? (
          <div className="mt-8">
            <EmptyState
              icon={FolderPlus}
              title="No projects yet"
              description="Group chats and knowledge into projects."
              cta={<Button href="/projects/new">New project</Button>}
            />
          </div>
        ) : null}
      </div>
    </div>
  );
}
