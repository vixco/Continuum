"use client";

import { useEffect, useRef } from "react";
import type { MemoryGraphData } from "@/lib/types";
import { EDGE_COLOR, GHOST_COLOR, LABEL_COLOR, NODE_COLORS } from "@/lib/memoryTheme";

interface GraphNodeObj {
  id: string;
  label: string;
  color: string;
  radius: number;
  ghost: boolean;
  status: string;
  x?: number;
  y?: number;
  fx?: number;
  fy?: number;
}

interface MemoryGraphProps {
  data: MemoryGraphData;
  selectedId: string | null;
  dimIds?: Set<string> | null;
  onSelect: (id: string | null) => void;
  onExpand: (id: string) => void;
  onGhostClick: (target: string) => void;
}

// force-graph has no bundled types compatible with its double-invocation
// factory API (`ForceGraph()(el)`); the shipped .d.ts models it as an ES6
// class constructor, which doesn't match the Kapsule-style runtime. Minimal
// structural typing instead.
type ForceGraphInstance = {
  (el: HTMLElement): ForceGraphInstance;
  graphData(d: {
    nodes: GraphNodeObj[];
    links: { source: string; target: string }[];
  }): ForceGraphInstance;
  nodeId(k: string): ForceGraphInstance;
  nodeCanvasObject(
    fn: (node: GraphNodeObj, ctx: CanvasRenderingContext2D, scale: number) => void
  ): ForceGraphInstance;
  nodePointerAreaPaint(
    fn: (node: GraphNodeObj, color: string, ctx: CanvasRenderingContext2D) => void
  ): ForceGraphInstance;
  linkColor(fn: () => string): ForceGraphInstance;
  linkWidth(n: number): ForceGraphInstance;
  onNodeClick(fn: (node: GraphNodeObj) => void): ForceGraphInstance;
  onNodeDragEnd(fn: (node: GraphNodeObj) => void): ForceGraphInstance;
  onBackgroundClick(fn: () => void): ForceGraphInstance;
  width(n: number): ForceGraphInstance;
  height(n: number): ForceGraphInstance;
  autoPauseRedraw(b: boolean): ForceGraphInstance;
  backgroundColor(c: string): ForceGraphInstance;
  centerAt(x?: number, y?: number, ms?: number): ForceGraphInstance;
  zoom(k?: number, ms?: number): ForceGraphInstance;
  _destructor?: () => void;
};

export function MemoryGraph({
  data,
  selectedId,
  dimIds,
  onSelect,
  onExpand,
  onGhostClick,
}: MemoryGraphProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const graphRef = useRef<ForceGraphInstance | null>(null);
  const lastClick = useRef<{ id: string; at: number }>({ id: "", at: 0 });
  const propsRef = useRef({ selectedId, dimIds, onSelect, onExpand, onGhostClick });
  propsRef.current = { selectedId, dimIds, onSelect, onExpand, onGhostClick };

  // init once
  useEffect(() => {
    let disposed = false;
    void import("force-graph").then((mod) => {
      if (disposed || !containerRef.current) return;
      const ForceGraph = mod.default as unknown as () => ForceGraphInstance;
      const g = ForceGraph()(containerRef.current)
        .nodeId("id")
        .autoPauseRedraw(false) // candidate pulse needs continuous redraw
        .backgroundColor("rgba(0,0,0,0)")
        .linkColor(() => EDGE_COLOR)
        .linkWidth(1)
        .nodeCanvasObject((node, ctx, scale) => {
          const { selectedId: sel, dimIds: dims } = propsRef.current;
          const dimmed = dims && dims.size > 0 && !dims.has(node.id);
          const faded = node.status === "superseded" || node.status === "archived";
          ctx.globalAlpha = dimmed ? 0.15 : faded ? 0.4 : 1;
          const r = node.radius;
          ctx.beginPath();
          ctx.arc(node.x ?? 0, node.y ?? 0, r, 0, 2 * Math.PI);
          if (node.ghost) {
            ctx.strokeStyle = GHOST_COLOR;
            ctx.lineWidth = 1.5;
            ctx.stroke();
          } else {
            ctx.fillStyle = node.color;
            ctx.fill();
          }
          if (node.status === "candidate") {
            const pulse = 0.5 + 0.5 * Math.sin(Date.now() / 300);
            ctx.beginPath();
            ctx.arc(node.x ?? 0, node.y ?? 0, r + 2 + pulse * 2, 0, 2 * Math.PI);
            ctx.strokeStyle = node.color;
            ctx.globalAlpha = (dimmed ? 0.15 : 0.5) * pulse;
            ctx.stroke();
            ctx.globalAlpha = dimmed ? 0.15 : 1;
          }
          if (node.id === sel) {
            ctx.beginPath();
            ctx.arc(node.x ?? 0, node.y ?? 0, r + 3, 0, 2 * Math.PI);
            ctx.strokeStyle = "#ffffff";
            ctx.lineWidth = 1;
            ctx.stroke();
          }
          if (scale > 1.4 || node.id === sel) {
            ctx.font = `${Math.max(10 / scale, 2)}px sans-serif`;
            ctx.textAlign = "center";
            ctx.fillStyle = LABEL_COLOR;
            ctx.fillText(node.label, node.x ?? 0, (node.y ?? 0) + r + 8 / scale);
          }
          ctx.globalAlpha = 1;
        })
        .nodePointerAreaPaint((node, color, ctx) => {
          ctx.fillStyle = color;
          ctx.beginPath();
          ctx.arc(node.x ?? 0, node.y ?? 0, node.radius + 4, 0, 2 * Math.PI);
          ctx.fill();
        })
        .onNodeClick((node) => {
          const p = propsRef.current;
          if (node.ghost) {
            p.onGhostClick(node.label);
            return;
          }
          const now = Date.now();
          if (lastClick.current.id === node.id && now - lastClick.current.at < 300) {
            p.onExpand(node.id);
          } else {
            p.onSelect(node.id);
          }
          lastClick.current = { id: node.id, at: now };
        })
        .onNodeDragEnd((node) => {
          node.fx = node.x;
          node.fy = node.y;
        })
        .onBackgroundClick(() => propsRef.current.onSelect(null));
      graphRef.current = g;

      const ro = new ResizeObserver(() => {
        if (!containerRef.current || !graphRef.current) return;
        graphRef.current.width(containerRef.current.clientWidth);
        graphRef.current.height(containerRef.current.clientHeight);
      });
      ro.observe(containerRef.current);
      return () => ro.disconnect();
    });
    return () => {
      disposed = true;
      graphRef.current?._destructor?.();
      graphRef.current = null;
    };
  }, []);

  // feed data
  useEffect(() => {
    const g = graphRef.current;
    if (!g) return;
    const nodes: GraphNodeObj[] = [
      ...data.nodes.map((n) => ({
        id: n.id,
        label: n.title,
        color: NODE_COLORS[n.type],
        radius: 3 + n.importance * 6,
        ghost: false,
        status: n.status,
      })),
      ...data.ghosts.map((gh) => ({
        id: `ghost:${gh.target}`,
        label: gh.target,
        color: GHOST_COLOR,
        radius: 3,
        ghost: true,
        status: "ghost",
      })),
    ];
    const ids = new Set(nodes.map((n) => n.id));
    // Ghosts float unlinked: the graph payload deliberately omits per-ghost
    // from-ids (see plan self-review — this is per plan, not an oversight).
    const links = data.edges
      .filter((e) => ids.has(e.from) && ids.has(e.to))
      .map((e) => ({ source: e.from, target: e.to }));
    g.graphData({ nodes, links });
  }, [data]);

  return <div ref={containerRef} className="h-full w-full" />;
}
