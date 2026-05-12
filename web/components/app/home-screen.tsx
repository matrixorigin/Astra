'use client';

import { BookOpen, Code2, Coffee, Lightbulb, PenLine } from 'lucide-react';
import { useRouter } from 'next/navigation';
import { useState } from 'react';
import { Composer } from '@/components/app/composer';
import { createChat } from '@/lib/api/chats';
import { isAuthRequiredError } from '@/lib/api/errors';
import { greetingFor } from '@/lib/utils/time';

const suggestions = [
  { icon: PenLine, label: 'Write', prompt: 'Help me write a crisp project update about ' },
  { icon: BookOpen, label: 'Learn', prompt: 'Teach me the key tradeoffs in ' },
  { icon: Code2, label: 'Code', prompt: 'Help me implement and test ' },
  { icon: Coffee, label: 'Life stuff', prompt: 'Help me plan ' },
  { icon: Lightbulb, label: 'Choice', prompt: 'Pick a useful next task for this workspace.' },
];

export function HomeScreen() {
  const router = useRouter();
  const [initialValue, setInitialValue] = useState('');
  const [busy, setBusy] = useState(false);

  return (
    <div className="flex h-full overflow-y-auto overscroll-contain flex-col items-center justify-center px-6 py-12">
      <h1 className="text-center text-4xl font-semibold tracking-normal">
        {greetingFor()}, Astra user
      </h1>

      <Composer
        key={initialValue}
        initialValue={initialValue}
        disabled={busy}
        className="mt-10 w-full max-w-composer"
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
              projectId: null,
            });
            router.replace(`/chats/${result.chatId}`);
          } catch (error) {
            if (isAuthRequiredError(error)) {
              router.push('/login?next=/');
              return;
            }
            throw error;
          } finally {
            setBusy(false);
          }
        }}
      />

      <div className="mt-5 flex max-w-composer flex-wrap justify-center gap-2">
        {suggestions.map((item) => (
          <button
            key={item.label}
            type="button"
            onClick={() => setInitialValue(item.prompt)}
            className="inline-flex h-9 items-center gap-2 rounded-full border border-border bg-surface px-3 text-sm text-text-secondary hover:bg-surface-muted hover:text-text"
          >
            <item.icon className="size-4" />
            {item.label}
          </button>
        ))}
      </div>
    </div>
  );
}
