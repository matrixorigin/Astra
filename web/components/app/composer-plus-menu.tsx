'use client';

import {
  AlertTriangle,
  ChevronLeft,
  Check,
  FilePlus2,
  Globe,
  HardDrive,
  Image,
  Monitor,
  Plug,
  Puzzle,
  RefreshCw,
  SlidersHorizontal,
  SquarePlus,
} from 'lucide-react';
import type { ComponentType, ReactNode } from 'react';
import { useState } from 'react';
import { Popover } from '@/components/ui/popover';
import { IconButton } from '@/components/ui/icon-button';
import { SkillPickerPanel } from '@/components/app/skill-picker-panel';
import type { EdgeStatusResponse, WorkspaceSelection } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

type EdgeWorkspace = EdgeStatusResponse['edges'][number];
type EdgeWorkspaceSelection = Extract<
  WorkspaceSelection,
  { kind: 'edge_workspace' }
>;
type MenuPanel = 'main' | 'skills' | 'environment';
type EnvironmentPickerProps = {
  workspaceSelection?: WorkspaceSelection | null;
  edgeWorkspaces?: EdgeStatusResponse['edges'];
  edgeWorkspacesLoading?: boolean;
  edgeWorkspacesError?: string | null;
  onWorkspaceSelectionChange?: (selection: WorkspaceSelection | null) => void;
  onRefreshEdgeWorkspaces?: () => void;
};

function Row({
  icon: Icon,
  label,
  description,
  disabled,
  selected,
  trailing,
  onClick,
}: {
  icon: ComponentType<{ className?: string }>;
  label: string;
  description?: string;
  disabled?: boolean;
  selected?: boolean;
  trailing?: ReactNode;
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
      <span className="min-w-0 flex-1">
        <span className="block truncate text-text">{label}</span>
        {description ? (
          <span className="block truncate text-xs leading-4 text-text-muted">
            {description}
          </span>
        ) : null}
      </span>
      {selected ? <Check className="size-4 shrink-0 text-accent" /> : trailing}
    </button>
  );
}

function edgeSelection(edge: EdgeWorkspace): EdgeWorkspaceSelection | null {
  const edgeAgentId = edge.edge_agent_id?.trim();
  const cwd = edge.workspace_dir?.trim();
  if (!edgeAgentId || !cwd) {
    return null;
  }
  return {
    kind: 'edge_workspace',
    edgeAgentId,
    displayName: edge.hostname?.trim() || edgeAgentId,
    cwd,
  };
}

function edgeDisplayName(edge: EdgeWorkspace) {
  return edge.hostname?.trim() || edge.edge_agent_id || 'Edge workspace';
}

function normalizeSlashPath(path: string) {
  const absolute = path.replace(/\\/g, '/').replace(/\/+/g, '/');
  const parts: string[] = [];
  for (const part of absolute.split('/')) {
    if (!part || part === '.') {
      continue;
    }
    if (part === '..') {
      parts.pop();
      continue;
    }
    parts.push(part);
  }
  return absolute.startsWith('/') ? `/${parts.join('/')}` : parts.join('/');
}

function sameSelection(
  left: WorkspaceSelection | null | undefined,
  right: WorkspaceSelection | null | undefined,
) {
  if (!left && !right) {
    return true;
  }
  if (!left || !right || left.kind !== right.kind) {
    return false;
  }
  if (left.kind === 'server_sandbox') {
    return true;
  }
  return (
    right.kind === 'edge_workspace' &&
    left.edgeAgentId === right.edgeAgentId &&
    left.cwd === right.cwd
  );
}

function edgeMatchesSelection(
  edge: EdgeWorkspace,
  selection: WorkspaceSelection | null | undefined,
) {
  if (selection?.kind !== 'edge_workspace') {
    return false;
  }
  const edgeCwd = edge.workspace_dir?.trim();
  if (!edgeCwd) {
    return false;
  }
  if (edge.edge_agent_id === selection.edgeAgentId) {
    return true;
  }
  if (normalizeSlashPath(edgeCwd) !== normalizeSlashPath(selection.cwd)) {
    return false;
  }
  const displayName = selection.displayName?.trim();
  return !displayName || edge.hostname?.trim() === displayName;
}

function workspaceLabel(selection: WorkspaceSelection | null | undefined) {
  if (!selection) {
    return 'Web';
  }
  if (selection.kind === 'server_sandbox') {
    return 'Server sandbox';
  }
  return selection.displayName?.trim() || selection.edgeAgentId;
}

function edgeIsConnected(
  selection: WorkspaceSelection | null | undefined,
  edges: EdgeWorkspace[],
) {
  if (selection?.kind !== 'edge_workspace') {
    return true;
  }
  return edges.some((edge) => edgeMatchesSelection(edge, selection));
}

function environmentSummary(
  selection: WorkspaceSelection | null | undefined,
  edges: EdgeWorkspace[],
) {
  const unavailable =
    selection?.kind === 'edge_workspace' && !edgeIsConnected(selection, edges);
  if (!selection) {
    return {
      label: 'Web',
      detail: 'No workspace',
      unavailable: false,
      icon: Monitor,
      connected: false,
    };
  }
  if (selection.kind === 'server_sandbox') {
    return {
      label: 'Server sandbox',
      detail: 'Server workspace',
      unavailable: false,
      icon: HardDrive,
      connected: true,
    };
  }
  return {
    label: unavailable ? 'Edge offline' : workspaceLabel(selection),
    detail: selection.cwd,
    unavailable,
    icon: HardDrive,
    connected: !unavailable,
  };
}

function EnvironmentPanel({
  workspaceSelection,
  edgeWorkspaces = [],
  edgeWorkspacesLoading = false,
  edgeWorkspacesError = null,
  onWorkspaceSelectionChange,
  onRefreshEdgeWorkspaces,
  onBack,
  onSelected,
}: EnvironmentPickerProps & {
  onBack?: () => void;
  onSelected?: () => void;
}) {
  const edgeRows = edgeWorkspaces
    .map((edge) => ({ edge, selection: edgeSelection(edge) }))
    .filter(
      (item): item is {
        edge: EdgeWorkspace;
        selection: EdgeWorkspaceSelection;
      } =>
        Boolean(item.selection),
    );
  const currentEdgeUnavailable =
    workspaceSelection?.kind === 'edge_workspace' &&
    !edgeIsConnected(workspaceSelection, edgeWorkspaces);

  function selectEnvironment(selection: WorkspaceSelection | null) {
    onWorkspaceSelectionChange?.(selection);
    onSelected?.();
  }

  return (
    <>
      {onBack ? (
        <button
          type="button"
          onClick={onBack}
          className="mb-1 flex w-full items-center gap-2 rounded-control px-2 py-2 text-sm font-medium text-text hover:bg-surface-muted"
        >
          <ChevronLeft className="size-4 text-text-muted" />
          Environment
        </button>
      ) : (
        <div className="mb-1 px-3 py-2 text-sm font-medium text-text">
          Environment
        </div>
      )}
      <Row
        icon={Monitor}
        label="Web"
        description="Chat, web, memory, planning"
        selected={!workspaceSelection}
        onClick={() => selectEnvironment(null)}
      />
      {workspaceSelection?.kind === 'edge_workspace' && currentEdgeUnavailable ? (
        <div className="my-2 rounded-control border border-warning/30 bg-warning/10 px-3 py-2 text-xs leading-5 text-text-secondary">
          <div className="flex items-center gap-2 font-medium text-text">
            <AlertTriangle className="size-3.5 text-warning" />
            Bound edge is offline
          </div>
          <div className="mt-1 truncate">
            {workspaceLabel(workspaceSelection)} · {workspaceSelection.cwd}
          </div>
        </div>
      ) : null}
      <div className="my-1 border-t border-border" />
      <div className="px-3 pb-1 pt-2 text-[11px] font-medium uppercase tracking-wide text-text-muted">
        Edge workspaces
      </div>
      {edgeRows.map(({ edge, selection }) => (
        <Row
          key={`${selection.edgeAgentId}:${selection.cwd}`}
          icon={HardDrive}
          label={edgeDisplayName(edge)}
          description={selection.cwd}
          selected={
            sameSelection(workspaceSelection, selection) ||
            edgeMatchesSelection(edge, workspaceSelection)
          }
          onClick={() => selectEnvironment(selection)}
        />
      ))}
      {!edgeRows.length ? (
        <div className="px-3 py-2 text-sm text-text-muted">
          No edge workspace connected.
        </div>
      ) : null}
      {edgeWorkspacesError ? (
        <div className="mx-3 my-2 rounded-control bg-danger/10 px-3 py-2 text-xs leading-5 text-danger">
          {edgeWorkspacesError}
        </div>
      ) : null}
      <div className="my-1 border-t border-border" />
      <Row
        icon={RefreshCw}
        label={edgeWorkspacesLoading ? 'Refreshing...' : 'Refresh environments'}
        disabled={edgeWorkspacesLoading}
        onClick={onRefreshEdgeWorkspaces}
      />
    </>
  );
}

export function ComposerEnvironmentChip({
  workspaceSelection,
  edgeWorkspaces = [],
  edgeWorkspacesLoading = false,
  edgeWorkspacesError = null,
  onWorkspaceSelectionChange,
  onRefreshEdgeWorkspaces,
}: EnvironmentPickerProps) {
  const [open, setOpen] = useState(false);
  const summary = environmentSummary(workspaceSelection, edgeWorkspaces);
  const Icon = summary.icon;

  return (
    <Popover
      open={open}
      onOpenChange={setOpen}
      trigger={
        <button
          type="button"
          aria-label={`Environment: ${summary.label}`}
          className={cn(
            'inline-flex h-9 min-w-0 max-w-[18rem] shrink items-center gap-2 rounded-control border border-border bg-bg px-2.5 text-left text-xs text-text-secondary hover:bg-surface-muted hover:text-text',
            summary.unavailable && 'border-warning/30 bg-warning/10 text-warning',
          )}
        >
          <span
            className={cn(
              'size-1.5 shrink-0 rounded-full',
              summary.unavailable
                ? 'bg-warning'
                : summary.connected
                  ? 'bg-success'
                  : 'bg-text-muted',
            )}
          />
          <Icon className="size-3.5 shrink-0" />
          <span className="min-w-0">
            <span className="block truncate font-medium leading-4 text-text">
              {summary.label}
            </span>
            <span className="block truncate leading-3 text-text-muted">
              {summary.detail}
            </span>
          </span>
        </button>
      }
      className="w-80"
    >
      <EnvironmentPanel
        workspaceSelection={workspaceSelection}
        edgeWorkspaces={edgeWorkspaces}
        edgeWorkspacesLoading={edgeWorkspacesLoading}
        edgeWorkspacesError={edgeWorkspacesError}
        onWorkspaceSelectionChange={onWorkspaceSelectionChange}
        onRefreshEdgeWorkspaces={onRefreshEdgeWorkspaces}
        onSelected={() => setOpen(false)}
      />
    </Popover>
  );
}

export function ComposerPlusMenu({
  inProject,
  webSearch,
  onWebSearchChange,
  activeSkills,
  onActiveSkillsChange,
  workspaceSelection,
  edgeWorkspaces = [],
  edgeWorkspacesLoading = false,
  edgeWorkspacesError = null,
  onWorkspaceSelectionChange,
  onRefreshEdgeWorkspaces,
}: {
  inProject?: boolean;
  webSearch: boolean;
  onWebSearchChange: (value: boolean) => void;
  activeSkills: string[];
  onActiveSkillsChange: (skills: string[]) => void;
  workspaceSelection?: WorkspaceSelection | null;
  edgeWorkspaces?: EdgeStatusResponse['edges'];
  edgeWorkspacesLoading?: boolean;
  edgeWorkspacesError?: string | null;
  onWorkspaceSelectionChange?: (selection: WorkspaceSelection | null) => void;
  onRefreshEdgeWorkspaces?: () => void;
}) {
  const [panel, setPanel] = useState<MenuPanel>('main');
  const environmentName = environmentSummary(
    workspaceSelection,
    edgeWorkspaces,
  ).label;

  return (
    <Popover
      trigger={<IconButton icon={SquarePlus} label="Open add menu" />}
      className={panel === 'skills' ? 'w-auto p-2' : 'w-80'}
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
      ) : panel === 'environment' ? (
        <EnvironmentPanel
          workspaceSelection={workspaceSelection}
          edgeWorkspaces={edgeWorkspaces}
          edgeWorkspacesLoading={edgeWorkspacesLoading}
          edgeWorkspacesError={edgeWorkspacesError}
          onWorkspaceSelectionChange={onWorkspaceSelectionChange}
          onRefreshEdgeWorkspaces={onRefreshEdgeWorkspaces}
          onBack={() => setPanel('main')}
          onSelected={() => setPanel('main')}
        />
      ) : (
        <>
          <Row icon={FilePlus2} label="Add files or photos" disabled />
          <Row icon={Image} label="Take a screenshot" disabled />
          {inProject ? null : <Row icon={SquarePlus} label="Add to project" disabled />}
          <div className="my-1 border-t border-border" />
          <Row
            icon={Monitor}
            label="Environment"
            onClick={() => setPanel('environment')}
            trailing={
              <span className="max-w-28 truncate rounded-full bg-surface-muted px-2 py-0.5 text-xs text-text-muted">
                {environmentName}
              </span>
            }
          />
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
