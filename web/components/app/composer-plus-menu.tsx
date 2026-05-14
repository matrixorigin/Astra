'use client';

import {
  Check,
  FilePlus2,
  Globe,
  Image,
  Plug,
  Puzzle,
  SlidersHorizontal,
  SquarePlus,
} from 'lucide-react';
import { useState } from 'react';
import { Popover } from '@/components/ui/popover';
import { IconButton } from '@/components/ui/icon-button';
import { SkillPickerPanel } from '@/components/app/skill-picker-panel';

function Row({
  icon: Icon,
  label,
  disabled,
  trailing,
  onClick,
}: {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  disabled?: boolean;
  trailing?: React.ReactNode;
  onClick?: () => void;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className="flex w-full items-center gap-3 rounded-control px-3 py-2 text-left text-sm hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-40"
    >
      <Icon className="size-4 text-text-muted" />
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {trailing}
    </button>
  );
}

export function ComposerPlusMenu({
  inProject,
  webSearch,
  onWebSearchChange,
  activeSkills,
  onActiveSkillsChange,
}: {
  inProject?: boolean;
  webSearch: boolean;
  onWebSearchChange: (value: boolean) => void;
  activeSkills: string[];
  onActiveSkillsChange: (skills: string[]) => void;
}) {
  const [panel, setPanel] = useState<'main' | 'skills'>('main');

  return (
    <Popover
      trigger={<IconButton icon={SquarePlus} label="Open add menu" />}
      className={panel === 'skills' ? 'w-auto p-2' : 'w-72'}
      onOpenChange={(open) => {
        if (!open) {
          setPanel('main');
        }
      }}
    >
      {panel === 'skills' ? (
        <SkillPickerPanel
          selected={activeSkills}
          onChange={onActiveSkillsChange}
          onBack={() => setPanel('main')}
        />
      ) : (
        <>
          <Row icon={FilePlus2} label="Add files or photos" disabled />
          <Row icon={Image} label="Take a screenshot" disabled />
          {inProject ? null : <Row icon={SquarePlus} label="Add to project" disabled />}
          <div className="my-1 border-t border-border" />
          <Row
            icon={Puzzle}
            label="Skills"
            onClick={() => setPanel('skills')}
            trailing={activeSkills.length ? (
              <span className="rounded-full bg-surface-muted px-2 py-0.5 text-xs text-text-muted">
                {activeSkills.length}
              </span>
            ) : null}
          />
          <Row icon={Plug} label="Add connectors" disabled />
          <Row
            icon={Globe}
            label="Web search"
            onClick={() => onWebSearchChange(!webSearch)}
            trailing={webSearch ? <Check className="size-4 text-accent" /> : null}
          />
          <Row icon={SlidersHorizontal} label="Use style" disabled />
        </>
      )}
    </Popover>
  );
}
