'use client';

import { Folder, Plus } from 'lucide-react';
import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { ListItem } from '@/components/ui/list-item';
import { Modal } from '@/components/ui/modal';
import { SearchField } from '@/components/ui/search-field';
import { listProjects } from '@/lib/api/projects';
import { updateChatProject } from '@/lib/api/chats';
import type { ProjectSummary } from '@/lib/api/types';
import { useDebouncedValue } from '@/hooks/use-debounced-value';

export function MoveChatModal({
  open,
  chatId,
  currentProjectId,
  onOpenChange,
  onMoved,
}: {
  open: boolean;
  chatId: string;
  currentProjectId: string | null;
  onOpenChange: (open: boolean) => void;
  onMoved: () => void;
}) {
  const [query, setQuery] = useState('');
  const debounced = useDebouncedValue(query, 250);
  const [projects, setProjects] = useState<ProjectSummary[]>([]);

  useEffect(() => {
    if (!open) {
      return;
    }
    listProjects({ q: debounced, limit: 20 }).then((result) => setProjects(result.items)).catch(() => setProjects([]));
  }, [debounced, open]);

  async function move(projectId: string | null) {
    await updateChatProject(chatId, projectId);
    onMoved();
    onOpenChange(false);
  }

  return (
    <Modal open={open} onOpenChange={onOpenChange} title="Move chat" width={480}>
      <div className="p-4">
        <SearchField value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search projects..." />
        <div className="mt-3 max-h-72 space-y-1 overflow-y-auto">
          {projects.map((project) => (
            <ListItem
              key={project.id}
              icon={Folder}
              title={project.name}
              subtitle={project.description ?? 'Private project'}
              onClick={() => move(project.id)}
              active={currentProjectId === project.id}
            />
          ))}
        </div>
        <div className="mt-4 flex flex-col gap-2 border-t border-border pt-4">
          <Button href="/projects/new" variant="ghost" leadingIcon={Plus}>Create new project</Button>
          {currentProjectId ? (
            <Button variant="ghost" onClick={() => move(null)}>Remove from current project</Button>
          ) : null}
        </div>
      </div>
    </Modal>
  );
}
