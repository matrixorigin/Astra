'use client';

import { FolderKanban, Star } from 'lucide-react';
import { useState } from 'react';
import { Card } from '@/components/ui/card';
import { IconButton } from '@/components/ui/icon-button';
import { setProjectStar } from '@/lib/api/projects';
import type { ProjectSummary } from '@/lib/api/types';
import { relativeTime } from '@/lib/utils/time';

export function ProjectCard({ project }: { project: ProjectSummary }) {
  const [starred, setStarred] = useState(project.starred);

  async function toggleStar(event: React.MouseEvent) {
    event.preventDefault();
    event.stopPropagation();
    const next = !starred;
    setStarred(next);
    try {
      await setProjectStar(project.id, next);
    } catch {
      setStarred(!next);
    }
  }

  return (
    <Card interactive href={`/projects/${project.id}`} className="flex min-h-[154px] flex-col justify-between pr-12">
      <div className="flex items-start gap-3">
        <span className="flex size-9 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
          <FolderKanban className="size-4" />
        </span>
        <div className="min-w-0">
        <h3 className="line-clamp-2 text-base font-semibold text-text">{project.name}</h3>
        {project.description ? (
            <p className="mt-1.5 line-clamp-2 text-sm leading-5 text-text-secondary">{project.description}</p>
        ) : null}
        </div>
      </div>
      <div className="flex items-center gap-2 text-xs text-text-muted">
        <span className="rounded-full bg-surface-muted px-2 py-1 capitalize">
          {project.visibility}
        </span>
        <span>Updated {relativeTime(project.updatedAt)}</span>
      </div>
      <IconButton
        icon={Star}
        label={starred ? 'Unstar project' : 'Star project'}
        active={starred}
        onClick={toggleStar}
        className="absolute right-3 top-3"
      />
    </Card>
  );
}
