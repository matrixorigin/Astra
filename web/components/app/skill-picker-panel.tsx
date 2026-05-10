'use client';

import { ArrowLeft, Check, Loader2, Search } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { listSkills } from '@/lib/api/skills';
import type { SkillSummary } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

type SkillPickerPanelProps = {
  selected: string[];
  onChange: (skills: string[]) => void;
  onBack: () => void;
};

const PAGE_SIZE = 100;

function skillSubtitle(skill: SkillSummary) {
  const parts = [
    skill.source,
    skill.category,
    skill.version ? `v${skill.version}` : null,
    skill.status,
  ].filter((part): part is string => Boolean(part));
  return parts.join(' · ');
}

function selectedSet(skills: string[]) {
  return new Set(skills.map((skill) => skill.trim()).filter(Boolean));
}

export function SkillPickerPanel({ selected, onChange, onBack }: SkillPickerPanelProps) {
  const [items, setItems] = useState<SkillSummary[]>([]);
  const [nextOffset, setNextOffset] = useState<number | null>(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState('');

  const selectedNames = useMemo(() => selectedSet(selected), [selected]);
  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) {
      return items;
    }
    return items.filter((skill) => {
      const haystack = `${skill.name} ${skill.description ?? ''} ${skill.source ?? ''} ${skill.category ?? ''}`.toLowerCase();
      return haystack.includes(needle);
    });
  }, [items, query]);

  const loadPage = useCallback(async (offset: number) => {
    setLoading(true);
    setError(null);
    try {
      const response = await listSkills({ limit: PAGE_SIZE, offset });
      setItems((current) => {
        const byName = new Map(current.map((skill) => [skill.name, skill]));
        for (const skill of response.items) {
          byName.set(skill.name, skill);
        }
        return [...byName.values()].sort((left, right) => left.name.localeCompare(right.name));
      });
      setNextOffset(response.nextOffset);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load skills.');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadPage(0);
  }, [loadPage]);

  function toggle(skillName: string) {
    const next = selectedSet(selected);
    if (next.has(skillName)) {
      next.delete(skillName);
    } else {
      next.add(skillName);
    }
    onChange([...next].sort((left, right) => left.localeCompare(right)));
  }

  return (
    <div className="w-96 max-w-[calc(100vw-2rem)]">
      <div className="flex items-center gap-2 px-1 pb-2">
        <button
          type="button"
          onClick={onBack}
          className="flex size-8 shrink-0 items-center justify-center rounded-control text-text-muted hover:bg-surface-muted hover:text-text"
          aria-label="Back to add menu"
        >
          <ArrowLeft className="size-4" />
        </button>
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium text-text">Skills</p>
          <p className="text-xs text-text-muted">{selected.length} selected for this turn</p>
        </div>
      </div>

      <label className="flex h-9 items-center gap-2 rounded-control border border-border bg-surface px-3 focus-within:border-accent">
        <Search className="size-4 shrink-0 text-text-muted" />
        <input
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search loaded skills..."
          className="min-w-0 flex-1 bg-transparent text-sm text-text outline-none placeholder:text-text-muted"
        />
      </label>

      {selected.length ? (
        <div className="mt-2 flex max-h-20 flex-wrap gap-1 overflow-y-auto">
          {selected.map((skill) => (
            <button
              key={skill}
              type="button"
              onClick={() => toggle(skill)}
              className="rounded-full bg-surface-muted px-2 py-1 text-xs text-text-secondary hover:bg-border"
            >
              {skill}
            </button>
          ))}
        </div>
      ) : null}

      {error ? (
        <div className="mt-3 rounded-control border border-danger/20 bg-danger/5 px-3 py-2 text-xs text-danger">
          {error}
        </div>
      ) : null}

      <div className="mt-2 max-h-[min(24rem,50vh)] space-y-1 overflow-y-auto pr-1">
        {filtered.map((skill) => {
          const checked = selectedNames.has(skill.name);
          return (
            <button
              key={skill.name}
              type="button"
              onClick={() => toggle(skill.name)}
              className={cn(
                'flex w-full items-start gap-3 rounded-control px-3 py-2 text-left hover:bg-surface-muted',
                checked && 'bg-surface-muted',
              )}
            >
              <span
                className={cn(
                  'mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-full border border-border-strong',
                  checked && 'border-accent bg-accent text-white',
                )}
              >
                {checked ? <Check className="size-3" /> : null}
              </span>
              <span className="min-w-0 flex-1">
                <span className="block truncate text-sm font-medium text-text">{skill.name}</span>
                {skill.description ? (
                  <span className="mt-0.5 line-clamp-2 block text-xs leading-4 text-text-secondary">
                    {skill.description}
                  </span>
                ) : null}
                {skillSubtitle(skill) ? (
                  <span className="mt-1 block truncate text-[11px] text-text-muted">
                    {skillSubtitle(skill)}
                  </span>
                ) : null}
              </span>
            </button>
          );
        })}

        {!loading && filtered.length === 0 ? (
          <p className="px-3 py-6 text-center text-sm text-text-muted">
            {items.length === 0 ? 'No skills are available.' : 'No loaded skills match this search.'}
          </p>
        ) : null}
      </div>

      <div className="mt-2 border-t border-border pt-2">
        {nextOffset !== null ? (
          <button
            type="button"
            disabled={loading}
            onClick={() => void loadPage(nextOffset)}
            className="flex h-9 w-full items-center justify-center gap-2 rounded-control text-sm text-text-secondary hover:bg-surface-muted disabled:opacity-50"
          >
            {loading ? <Loader2 className="size-4 animate-spin" /> : null}
            Load more skills
          </button>
        ) : (
          <p className="px-3 py-1 text-center text-xs text-text-muted">
            {loading ? 'Loading skills...' : `${items.length} skills loaded`}
          </p>
        )}
      </div>
    </div>
  );
}
