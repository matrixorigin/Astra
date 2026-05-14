'use client';

import { Edit3, FilePlus2, Plus } from 'lucide-react';
import { useRouter } from 'next/navigation';
import { useCallback, useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { IconButton } from '@/components/ui/icon-button';
import { PageHeader } from '@/components/ui/page-header';
import { Composer } from '@/components/app/composer';
import { InstructionsModal } from '@/components/app/instructions-modal';
import { KnowledgeCard } from '@/components/app/knowledge-card';
import { KnowledgeItem } from '@/components/app/knowledge-item';
import { ChatRow } from '@/components/app/chat-row';
import { createChat } from '@/lib/api/chats';
import { getProject, updateProject, uploadProjectFile } from '@/lib/api/projects';
import type { ProjectDetail as ProjectDetailType } from '@/lib/api/types';
import { relativeTime } from '@/lib/utils/time';
import { subscribeChatLifecycleChange } from '@/lib/chat-lifecycle-events';

export function ProjectDetail({ initial }: { initial: ProjectDetailType }) {
  const router = useRouter();
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [detail, setDetail] = useState(initial);
  const [instructionsOpen, setInstructionsOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    setDetail(await getProject(detail.project.id));
  }, [detail.project.id]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => subscribeChatLifecycleChange(() => {
    void refresh();
  }), [refresh]);

  return (
    <div className="grid h-full overflow-y-auto overscroll-contain grid-cols-[minmax(0,1fr)_var(--right-panel-width)] gap-0 max-lg:grid-cols-1">
      <section className="min-w-0 px-8 py-8">
        <button type="button" onClick={() => router.back()} className="mb-5 text-sm text-text-secondary hover:text-text">
          ← All projects
        </button>
        <PageHeader
          title={detail.project.name}
          description={detail.project.description ?? undefined}
          action={
            detail.chats[0] ? (
              <Button href={`/projects/${detail.project.id}/chats/${detail.chats[0].id}`} variant="ghost">
                Open latest
              </Button>
            ) : null
          }
        />

        <Composer
          disabled={busy}
          projectContext={{ projectId: detail.project.id }}
          className="mt-8 max-w-composer"
          placeholder={`Ask about ${detail.project.name}...`}
          onSubmit={async ({ text, options }) => {
            setBusy(true);
            try {
              const result = await createChat({
                message: text,
                model: options.model,
                options: {
                  webSearch: options.webSearch,
                  thinking: options.thinking,
                  activeSkills: options.activeSkills,
                },
                projectId: detail.project.id,
              });
              router.push(`/projects/${detail.project.id}/chats/${result.chatId}`);
            } finally {
              setBusy(false);
            }
          }}
        />

        <div className="mt-8">
          <h2 className="text-sm font-semibold text-text-secondary">Recent chats</h2>
          <div className="mt-3 space-y-2">
            {detail.chats.length ? (
              detail.chats.map((chat) => (
                <ChatRow
                  key={chat.id}
                  chatId={chat.id}
                  title={chat.title ?? 'Untitled'}
                  subtitle={`Last message ${relativeTime(chat.lastMessageAt)}`}
                  href={`/projects/${detail.project.id}/chats/${chat.id}`}
                  archived={Boolean(chat.archivedAt)}
                  afterMutationHref={`/projects/${detail.project.id}`}
                />
              ))
            ) : (
              <p className="rounded-card border border-border bg-surface px-4 py-6 text-sm text-text-muted">
                No chats in this project yet.
              </p>
            )}
          </div>
        </div>
      </section>

      <aside className="h-full overflow-y-auto border-l border-border bg-surface-muted/40 px-5 py-8 max-lg:border-l-0 max-lg:border-t">
        <div className="space-y-4">
          <KnowledgeCard title="Memory">
            <p className="text-sm text-text-secondary">
              {detail.project.memory ?? 'No memory has been generated for this project yet.'}
            </p>
          </KnowledgeCard>

          <KnowledgeCard
            title="Instructions"
            action={
              <IconButton
                icon={detail.project.instructions ? Edit3 : Plus}
                label={detail.project.instructions ? 'Edit instructions' : 'Add instructions'}
                onClick={() => setInstructionsOpen(true)}
              />
            }
          >
            <p className="whitespace-pre-wrap text-sm text-text-secondary">
              {detail.project.instructions ?? 'Add stable preferences for this project.'}
            </p>
          </KnowledgeCard>

          <KnowledgeCard
            title="Files"
            action={<IconButton icon={FilePlus2} label="Upload file" onClick={() => fileInputRef.current?.click()} />}
          >
            <input
              ref={fileInputRef}
              type="file"
              className="hidden"
              onChange={async (event) => {
                const file = event.target.files?.[0];
                if (!file) {
                  return;
                }
                await uploadProjectFile(detail.project.id, file);
                await refresh();
                event.target.value = '';
              }}
            />
            {detail.files.length ? (
              <div className="space-y-2">
                {detail.files.map((file) => <KnowledgeItem key={file.id} file={file} />)}
              </div>
            ) : (
              <p className="text-sm text-text-muted">Upload files to ground answers in project knowledge.</p>
            )}
          </KnowledgeCard>
        </div>
      </aside>

      <InstructionsModal
        open={instructionsOpen}
        initialValue={detail.project.instructions ?? ''}
        onOpenChange={setInstructionsOpen}
        onSave={async (instructions) => {
          const next = await updateProject(detail.project.id, { instructions });
          setDetail(next);
        }}
      />
    </div>
  );
}
