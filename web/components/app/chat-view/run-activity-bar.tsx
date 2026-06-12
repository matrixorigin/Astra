import { Bot, ClipboardList, Loader2, Terminal, type LucideIcon } from "lucide-react";

export type RunActivityMetricCount = {
  active: number;
  total: number;
  value: number;
  mode: "active" | "item";
};

export function runActivityMetricCount(
  active: number,
  total: number,
): RunActivityMetricCount {
  if (active > 0) {
    return { active, total, value: active, mode: "active" };
  }
  return { active, total, value: total, mode: "item" };
}

export function RunActivityBar({
  label,
  agents,
  tools,
  tasks,
  onOpenAgents,
  onOpenTasks,
  onOpenTools,
}: {
  label: string;
  agents: RunActivityMetricCount;
  tools: RunActivityMetricCount;
  tasks: RunActivityMetricCount;
  onOpenAgents: () => void;
  onOpenTasks: () => void;
  onOpenTools: () => void;
}) {
  return (
    <div className="mb-3 flex flex-wrap items-center gap-2 rounded-[14px] border border-border/70 bg-surface/95 px-3 py-2 text-xs text-text-muted shadow-[0_0.15rem_0.8rem_rgba(28,25,23,0.05)]">
      <span className="inline-flex min-w-0 items-center gap-2 font-medium text-text">
        <Loader2 className="size-3.5 animate-spin text-accent" />
        <span className="truncate">{label}</span>
      </span>
      <span
        className="inline-flex items-center gap-1 pl-0.5"
        aria-hidden="true"
      >
        <span className="size-1.5 animate-bounce rounded-full bg-text-muted" />
        <span
          className="size-1.5 animate-bounce rounded-full bg-text-muted"
          style={{ animationDelay: "120ms" }}
        />
        <span
          className="size-1.5 animate-bounce rounded-full bg-text-muted"
          style={{ animationDelay: "240ms" }}
        />
      </span>
      <span className="h-4 w-px bg-border" aria-hidden="true" />
      <RunActivityMetric
        icon={Bot}
        label="Agents"
        value={agents}
        onClick={onOpenAgents}
      />
      <RunActivityMetric
        icon={ClipboardList}
        label="Tasks"
        value={tasks}
        onClick={onOpenTasks}
      />
      <RunActivityMetric
        icon={Terminal}
        label="Tools"
        value={tools}
        onClick={onOpenTools}
      />
    </div>
  );
}

function RunActivityMetric({
  icon: Icon,
  label,
  value,
  onClick,
}: {
  icon: LucideIcon;
  label: string;
  value: RunActivityMetricCount;
  onClick: () => void;
}) {
  const unitLabel = value.mode === "active" ? "active" : "item";
  const pluralUnit =
    value.mode === "active" || value.value === 1 ? unitLabel : `${unitLabel}s`;
  const title =
    value.mode === "active"
      ? `Open ${label} (${value.active} active, ${value.total} total)`
      : `Open ${label} (${value.total} total)`;
  return (
    <button
      type="button"
      className="inline-flex min-w-0 items-center gap-1.5 rounded-full bg-bg px-2.5 py-1 font-medium text-text-secondary transition hover:bg-surface-muted hover:text-text focus:outline-none focus:ring-2 focus:ring-accent/30"
      onClick={onClick}
      aria-label={`Open ${label.toLowerCase()} work surface, ${value.value} ${pluralUnit}`}
      title={title}
    >
      <Icon className="size-3.5 text-text-muted" />
      <span className="tabular-nums text-text">{value.value}</span>
      <span className="hidden sm:inline">{label}</span>
      <span className="hidden text-text-muted xl:inline">{pluralUnit}</span>
    </button>
  );
}
