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
  vx?: number;
  vy?: number;
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

/** Builds the `{ nodes, links }` payload force-graph expects from raw
 * `MemoryGraphData`, carrying over each node's last-known simulated
 * position (and any user-pinned fx/fy) from `prevNodes` when a node with
 * the same id existed in the previous feed — otherwise a refresh (which
 * always builds fresh node literals) would reset the whole layout and
 * un-pin anything the user had dragged. */
function buildGraphPayload(
  data: MemoryGraphData,
  prevNodes: Map<string, GraphNodeObj>
): { nodes: GraphNodeObj[]; links: { source: string; target: string }[] } {
  const nodes: GraphNodeObj[] = [
    ...data.nodes.map((n) => {
      const prev = prevNodes.get(n.id);
      return {
        id: n.id,
        label: n.title,
        color: NODE_COLORS[n.type],
        radius: 3 + n.importance * 6,
        ghost: false,
        status: n.status,
        x: prev?.x,
        y: prev?.y,
        vx: prev?.vx,
        vy: prev?.vy,
        fx: prev?.fx,
        fy: prev?.fy,
      };
    }),
    ...data.ghosts.map((gh) => {
      const id = `ghost:${gh.target}`;
      const prev = prevNodes.get(id);
      return {
        id,
        label: gh.target,
        color: GHOST_COLOR,
        radius: 3,
        ghost: true,
        status: "ghost",
        x: prev?.x,
        y: prev?.y,
        vx: prev?.vx,
        vy: prev?.vy,
        fx: prev?.fx,
        fy: prev?.fy,
      };
    }),
  ];
  const ids = new Set(nodes.map((n) => n.id));
  // Ghosts float unlinked: the graph payload deliberately omits per-ghost
  // from-ids (see plan self-review — this is per plan, not an oversight).
  const links = data.edges
    .filter((e) => ids.has(e.from) && ids.has(e.to))
    .map((e) => ({ source: e.from, target: e.to }));
  return { nodes, links };
}

/** Feeds `data` into the live graph instance and refreshes `prevNodesRef`
 * with the just-fed node objects, so the *next* feed can carry their
 * (possibly since-simulated or since-dragged) positions forward. Module
 * scoped rather than a component closure so it isn't a reactive value the
 * effects below would need to list as a dependency. */
function feedGraph(
  g: ForceGraphInstance,
  data: MemoryGraphData,
  prevNodesRef: { current: Map<string, GraphNodeObj> }
) {
  const { nodes, links } = buildGraphPayload(data, prevNodesRef.current);
  prevNodesRef.current = new Map(nodes.map((n) => [n.id, n]));
  g.graphData({ nodes, links });
}

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
  // Latest `data`, kept current by the feed effect below regardless of
  // whether the graph instance exists yet — read once the async
  // force-graph import resolves so data arriving before init isn't lost.
  const dataRef = useRef(data);
  // Simulated positions/pins from the last feed, keyed by node id.
  const prevNodesRef = useRef<Map<string, GraphNodeObj>>(new Map());

  // init once
  useEffect(() => {
    let disposed = false;
    let ro: ResizeObserver | null = null;
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
      // `data` may have already changed (possibly more than once) while
      // this import was in flight — the feed effect below no-ops on every
      // one of those updates because graphRef.current was still null, so
      // without this the graph would stay permanently blank. `dataRef`
      // always holds the latest value regardless of init timing.
      feedGraph(g, dataRef.current, prevNodesRef);

      ro = new ResizeObserver(() => {
        if (!containerRef.current || !graphRef.current) return;
        graphRef.current.width(containerRef.current.clientWidth);
        graphRef.current.height(containerRef.current.clientHeight);
      });
      ro.observe(containerRef.current);
    });
    return () => {
      disposed = true;
      // `ro` is a plain closure variable (not a ref) so this cleanup —
      // which React *does* run, unlike the `.then()` callback's own return
      // value, which React never sees — can always reach whichever
      // ResizeObserver (if any) the async init actually created.
      ro?.disconnect();
      graphRef.current?._destructor?.();
      graphRef.current = null;
    };
  }, []);

  // feed data
  useEffect(() => {
    dataRef.current = data;
    const g = graphRef.current;
    if (!g) return;
    feedGraph(g, data, prevNodesRef);
  }, [data]);

  return <div ref={containerRef} className="h-full w-full" />;
}
