'use client';

import { useCallback, useMemo, useState } from 'react';
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  Panel,
  useNodesState,
  useEdgesState,
  type Node,
  type ColorMode,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';

import type { PlanGraphData } from '@/lib/graph/types';
import { buildFlowElements, statusColors } from '@/lib/graph/layout';
import { SubtaskNode } from './subtask-node';
import { DelegationNode } from './delegation-node';

const nodeTypes = {
  subtaskNode: SubtaskNode,
  delegationNode: DelegationNode,
};

interface PlanGraphProps {
  data: PlanGraphData;
}

export function PlanGraph({ data }: PlanGraphProps) {
  const { nodes: initialNodes, edges: initialEdges } = useMemo(
    () => buildFlowElements(data),
    [data],
  );

  const [nodes, , onNodesChange] = useNodesState(initialNodes);
  const [edges, , onEdgesChange] = useEdgesState(initialEdges);
  const [selectedNode, setSelectedNode] = useState<Node | null>(null);

  const onNodeClick = useCallback((_: React.MouseEvent, node: Node) => {
    setSelectedNode((prev) => (prev?.id === node.id ? null : node));
  }, []);

  const onPaneClick = useCallback(() => setSelectedNode(null), []);

  const progress = data.task.progressPct;
  const done = data.task.itemsDone;
  const total = data.task.itemsTotal;

  return (
    <div className="relative h-full w-full">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onNodeClick={onNodeClick}
        onPaneClick={onPaneClick}
        nodeTypes={nodeTypes}
        colorMode={'dark' as ColorMode}
        fitView
        fitViewOptions={{ padding: 0.3, maxZoom: 1.2 }}
        proOptions={{ hideAttribution: true }}
        minZoom={0.2}
        maxZoom={2}
      >
        <Background color="#1e293b" gap={24} size={1} />
        <Controls
          className="!bg-slate-900 !border-slate-700 !rounded-xl !shadow-xl [&>button]:!bg-slate-800 [&>button]:!border-slate-700 [&>button]:!text-slate-300 [&>button:hover]:!bg-slate-700"
        />
        <MiniMap
          nodeColor={(node: Node) => {
            const status = (node.data as Record<string, unknown>)?.status as string;
            return statusColors[status]?.border ?? '#334155';
          }}
          className="!bg-slate-900/80 !border-slate-700 !rounded-xl"
          maskColor="rgba(2, 6, 23, 0.7)"
        />

        {/* Header panel */}
        <Panel position="top-left" className="!m-3">
          <div className="rounded-xl border border-slate-700 bg-slate-900/90 px-4 py-3 backdrop-blur">
            <h3 className="text-sm font-semibold text-white">{data.task.title}</h3>
            <div className="mt-2 flex items-center gap-3 text-xs text-slate-400">
              <span className="capitalize">{data.task.status.replace('_', ' ')}</span>
              <span>•</span>
              <span>{done}/{total} subtasks</span>
              <span>•</span>
              <span>{progress}%</span>
            </div>
            <div className="mt-2 h-1.5 w-48 overflow-hidden rounded-full bg-slate-800">
              <div
                className="h-full rounded-full bg-sky-500 transition-all duration-500"
                style={{ width: `${progress}%` }}
              />
            </div>
          </div>
        </Panel>

        {/* Legend */}
        <Panel position="top-right" className="!m-3">
          <div className="rounded-xl border border-slate-700 bg-slate-900/90 px-4 py-3 backdrop-blur">
            <p className="text-xs font-medium text-slate-400 mb-2">Legend</p>
            <div className="space-y-1.5">
              {Object.entries(statusColors).map(([key, val]) => (
                <div key={key} className="flex items-center gap-2 text-xs">
                  <div
                    className="h-3 w-3 rounded-sm border"
                    style={{ backgroundColor: val.bg, borderColor: val.border }}
                  />
                  <span className="capitalize text-slate-300">{key.replace('_', ' ')}</span>
                </div>
              ))}
              <div className="flex items-center gap-2 text-xs mt-2 pt-2 border-t border-slate-700">
                <div className="h-3 w-3 rounded-sm border-2 border-dashed border-purple-500 bg-purple-950" />
                <span className="text-slate-300">Delegation</span>
              </div>
            </div>
          </div>
        </Panel>
      </ReactFlow>

      {selectedNode && (() => {
        const d = selectedNode.data as Record<string, string | number | undefined>;
        return (
          <div className="absolute right-3 top-1/2 z-10 w-72 -translate-y-1/2 rounded-xl border border-slate-700 bg-slate-900/95 p-4 shadow-2xl backdrop-blur">
            <div className="flex items-center justify-between">
              <h4 className="text-sm font-semibold text-white">
                {String(d.label ?? '')}
              </h4>
              <button
                type="button"
                onClick={() => setSelectedNode(null)}
                className="text-slate-500 hover:text-white text-lg leading-none"
                aria-label="Close detail panel"
              >
                ×
              </button>
            </div>
            <div className="mt-3 space-y-2 text-xs text-slate-400">
              <p>
                <span className="text-slate-500">Status: </span>
                <span className="capitalize text-white">
                  {String(d.status ?? '').replace('_', ' ')}
                </span>
              </p>
              {d.description ? (
                <p className="leading-relaxed">{String(d.description)}</p>
              ) : null}
              {d.acceptance ? (
                <p>
                  <span className="text-slate-500">Acceptance: </span>
                  {String(d.acceptance)}
                </p>
              ) : null}
              {d.effort ? (
                <p>
                  <span className="text-slate-500">Effort: </span>
                  {String(d.effort)}
                </p>
              ) : null}
              {Number(d.filesCount ?? 0) > 0 ? (
                <p>
                  <span className="text-slate-500">Files: </span>
                  {Number(d.filesCount)}
                </p>
              ) : null}
              {d.fromAgent ? (
                <>
                  <p>
                    <span className="text-slate-500">From: </span>
                    {String(d.fromAgent)}
                  </p>
                  <p>
                    <span className="text-slate-500">To: </span>
                    {String(d.toAgent ?? '')}
                  </p>
                </>
              ) : null}
            </div>
          </div>
        );
      })()}
    </div>
  );
}
