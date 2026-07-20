'use client';

import { Check, ChevronDown, Circle } from 'lucide-react';
import { useEffect, useState } from 'react';
import { Popover } from '@/components/ui/popover';
import { listModels, type ModelCatalogResponse } from '@/lib/api/models';
import type { ModelSummary } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

type CatalogLoadState =
  | { kind: 'loading' }
  | { kind: 'loaded'; catalog: ModelCatalogResponse }
  | { kind: 'error' };

function unavailableMessage(catalog: ModelCatalogResponse) {
  if (catalog.actions.includes('reconnect_device')) {
    return 'Reconnect this device in Model Access.';
  }
  if (catalog.actions.includes('contact_administrator')) {
    return 'Ask an administrator to enable a model.';
  }
  return 'No eligible models are available.';
}

export function ModelSwitcher({
  value,
  onChange,
  onModelAvailabilityChange,
  thinking,
  onThinkingChange,
}: {
  value: string;
  onChange: (value: string) => void;
  onModelAvailabilityChange?: (available: boolean) => void;
  thinking: boolean;
  onThinkingChange: (value: boolean) => void;
}) {
  const [catalogState, setCatalogState] = useState<CatalogLoadState>({
    kind: 'loading',
  });

  useEffect(() => {
    listModels()
      .then((catalog) => setCatalogState({ kind: 'loaded', catalog }))
      .catch(() => {
        setCatalogState({ kind: 'error' });
      });
  }, []);

  const catalog = catalogState.kind === 'loaded' ? catalogState.catalog : undefined;
  const models: ModelSummary[] = catalog?.items ?? [];
  const selected = models.find((model) => model.id === value);
  const defaultModel = catalog?.defaultOfferingId
    ? models.find((model) => model.id === catalog.defaultOfferingId)
    : undefined;
  const shouldSelectDefault = Boolean(defaultModel) && !value;
  const visibleSelected = selected ?? (shouldSelectDefault ? defaultModel : undefined);
  const modelUnavailable = Boolean(catalog) && Boolean(value) && !visibleSelected;

  useEffect(() => {
    if (!defaultModel || !shouldSelectDefault || value === defaultModel.id) {
      return;
    }
    onChange(defaultModel.id);
  }, [defaultModel, onChange, shouldSelectDefault, value]);

  useEffect(() => {
    onModelAvailabilityChange?.(
      Boolean(catalog) && Boolean(visibleSelected) && !modelUnavailable,
    );
  }, [catalog, modelUnavailable, onModelAvailabilityChange, visibleSelected]);

  return (
    <Popover
      align="end"
      trigger={
        <button
          type="button"
          data-invalid={modelUnavailable || undefined}
          title={modelUnavailable ? value : undefined}
          className="flex max-w-56 items-center gap-2 rounded-control px-2 py-1 text-sm text-text-secondary hover:bg-surface-muted hover:text-text"
        >
          <span className="truncate">
            {visibleSelected?.name ??
              (modelUnavailable
                ? 'Unavailable model'
                : catalogState.kind === 'loading'
                  ? 'Loading models…'
                  : catalogState.kind === 'error'
                    ? 'Model access unavailable'
                  : 'No models available')}
          </span>
          <ChevronDown className="size-4" />
        </button>
      }
      className="w-80 overflow-hidden p-0"
    >
      <div className="flex flex-col">
        <div className="max-h-[25vh] min-h-0 space-y-1 overflow-y-auto overscroll-contain p-2 pr-1">
          {catalogState.kind === 'error' ? (
            <div className="px-3 py-4 text-sm text-text-muted">
              Model Access could not be loaded. Sign in again or retry.
            </div>
          ) : null}
          {catalog && models.length === 0 ? (
            <div className="px-3 py-4 text-sm text-text-muted">
              {unavailableMessage(catalog)}
            </div>
          ) : null}
          {models.map((model) => {
            const checked = model.id === (visibleSelected?.id ?? value);
            return (
              <button
                key={model.id}
                type="button"
                onClick={() => onChange(model.id)}
                className="flex w-full items-start gap-2.5 rounded-control px-3 py-2 text-left hover:bg-surface-muted"
              >
                <Circle className={cn('mt-1 size-3', checked && 'fill-accent text-accent')} />
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-2 text-[13px] font-semibold leading-5">
                    {model.name}
                    {model.tier === 'upgrade' ? (
                      <span className="rounded-full bg-surface-muted px-2 py-0.5 text-xs text-text-muted">
                        Upgrade
                      </span>
                    ) : null}
                  </span>
                  <span className="mt-0.5 block text-[11px] leading-4 text-text-muted">{model.subtitle}</span>
                </span>
                {checked ? <Check className="size-4 text-accent" /> : null}
              </button>
            );
          })}
        </div>

        <div className="border-t border-border" />
        <button
          type="button"
          onClick={() => onThinkingChange(!thinking)}
          className="m-2 mt-1 flex shrink-0 items-center gap-3 rounded-control px-3 py-3 text-left hover:bg-surface-muted"
        >
          <span className={cn('h-4 w-8 rounded-full p-0.5', thinking ? 'bg-accent' : 'bg-border-strong')}>
            <span
              className={cn(
                'block size-3 rounded-full bg-white',
                thinking && 'translate-x-4',
              )}
            />
          </span>
          <span>
            <span className="block text-sm font-medium">Adaptive thinking</span>
            <span className="block text-xs text-text-muted">Thinks for more complex tasks</span>
          </span>
        </button>
      </div>
    </Popover>
  );
}
