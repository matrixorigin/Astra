'use client';

import { useRouter } from 'next/navigation';
import { useState } from 'react';
import { z } from 'zod';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { PageHeader } from '@/components/ui/page-header';
import { Textarea } from '@/components/ui/textarea';
import { createProject } from '@/lib/api/projects';

const schema = z.object({
  name: z.string().trim().min(1, 'Name is required').max(80),
  description: z.string().trim().max(280).optional(),
  instructions: z.string().trim().max(8000).optional(),
});

export function CreateProjectForm() {
  const router = useRouter();
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [instructions, setInstructions] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    const parsed = schema.safeParse({ name, description, instructions });
    if (!parsed.success) {
      setError(parsed.error.issues[0]?.message ?? 'Invalid project');
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const result = await createProject(parsed.data);
      router.replace(`/projects/${result.project.id}`);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create project');
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto w-full max-w-3xl">
        <PageHeader title="New project" description="Group related chats and knowledge into a durable workspace." />
        <form onSubmit={submit} className="mt-8 space-y-5">
          {error ? <div className="rounded-card border border-danger/20 bg-danger/5 px-4 py-3 text-sm text-danger">{error}</div> : null}
          <div>
            <label htmlFor="project-name" className="text-sm font-medium">Name</label>
            <Input id="project-name" value={name} onChange={(event) => setName(event.target.value)} maxLength={80} className="mt-2" />
          </div>
          <div>
            <label htmlFor="project-description" className="text-sm font-medium">Description</label>
            <Textarea id="project-description" value={description} onChange={(event) => setDescription(event.target.value)} maxLength={280} className="mt-2 min-h-20" />
          </div>
          <div>
            <label htmlFor="project-instructions" className="text-sm font-medium">Instructions</label>
            <Textarea id="project-instructions" value={instructions} onChange={(event) => setInstructions(event.target.value)} maxLength={8000} className="mt-2 min-h-44" />
          </div>
          <div className="flex justify-end gap-3">
            <Button type="button" variant="ghost" onClick={() => router.back()}>Cancel</Button>
            <Button type="submit" variant="primary" disabled={saving}>{saving ? 'Creating...' : 'Create project'}</Button>
          </div>
        </form>
      </div>
    </div>
  );
}
