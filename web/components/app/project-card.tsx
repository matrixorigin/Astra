'use client';

import { Star } from 'lucide-react';
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
    <Card interactive href={`/projects/${project.id}`} className="flex h-[140px] flex-col justify-between pr-12">
      <div>
        <h3 className="line-clamp-2 text-base font-medium">{project.name}</h3>
        {project.description ? (
          <p className="mt-2 line-clamp-2 text-sm text-text-secondary">{project.description}</p>
        ) : null}
      </div>
      <p className="text-sm text-text-secondary">Updated {relativeTime(project.updatedAt)}</p>
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
