"use client";

import type { ReactNode } from "react";
import {
  Activity,
  AlertTriangle,
  ArrowRight,
  Bot,
  Box,
  BrainCircuit,
  Check,
  CheckCircle2,
  CircleDot,
  Clock3,
  Code2,
  Database,
  FileText,
  FolderGit2,
  GitCommitHorizontal,
  Github,
  Goal,
  KeyRound,
  Layers3,
  Lightbulb,
  Play,
  Plus,
  RefreshCw,
  Rocket,
  Search,
  ShieldCheck,
  Sparkles,
  Target,
  TerminalSquare,
  Users,
  XCircle,
} from "lucide-react";

import { Badge, Button, Dot, Panel, Progress, Ring, StatCard, TextAction } from "./ui";
import type { UpdateInfo } from "@/lib/tauri";
import { ResourcePanel } from "@/components/continuum/ResourcePanel";

function PageHeader({
  title,
  subtitle,
  actions,
}: {
  title: string;
  subtitle: string;
  actions?: ReactNode;
}) {
  return (
    <div className="mb-3 flex min-h-[48px] items-start justify-between gap-6">
      <div>
        <h1 className="text-[25px] font-semibold tracking-[-0.03em] text-white">{title}</h1>
        <p className="mt-1 text-[12px] text-amber-400">{subtitle}</p>
      </div>
      {actions && <div className="mr-[170px] flex items-center gap-2">{actions}</div>}
    </div>
  );
}

const decisions = [
  "Adopt PostgreSQL for analytics store",
  "SimCharts API rate limit strategy",
  "Chart rendering: D3 + Canvas hybrid",
  "Auth: OAuth 2.1 with refresh tokens",
];

const blockers = [
  ["Data backfill for missing historical metrics", "High"],
  ["Permissions audit for production rollout", "Medium"],
  ["Chart export performance on large datasets", "Low"],
];

export function HomeScreen({ onNavigate }: { onNavigate: (tab: string) => void }) {
  return (
    <>
      <PageHeader
        title="Good morning, Toshan"
        subtitle="Here’s your command center. Continuum is ready to support your agents."
        actions={
          <label className="continuum-search hidden w-[510px] xl:flex">
            <Search size={15} />
            <input
              aria-label="Search Continuum"
              placeholder="Search across memory, projects, decisions..."
            />
            <kbd>/</kbd>
          </label>
        }
      />

      <div className="home-focus-layout grid gap-4 xl:relative xl:block">
        <Panel className="home-focus-panel relative min-h-[260px] overflow-hidden p-7 xl:mr-[376px] xl:h-[260px]">
          <div className="relative z-10 max-w-[53%]">
            <div className="continuum-eyebrow">Today’s focus</div>
            <h2 className="mt-2 text-[23px] font-medium text-white">SimCharts Rebuild</h2>
            <p className="mt-2 text-[12px] leading-5 text-white/50">
              Build a next-gen SimCharts platform with real-time data, intelligent context, and
              multi-agent collaboration.
            </p>
            <div className="mt-5 flex flex-wrap gap-2">
              <Badge tone="amber">● Active</Badge>
              <Badge>92% Context Health</Badge>
              <Badge>4 Agents Active</Badge>
            </div>
            <div className="mt-6 flex gap-2">
              <Button variant="primary">
                <Play size={14} /> Resume work
              </Button>
              <Button onClick={() => onNavigate("projects")}>
                <FolderGit2 size={14} /> Open project
              </Button>
            </div>
          </div>
          <div className="continuum-orbit" aria-hidden="true">
            <span className="orbit orbit-a" />
            <span className="orbit orbit-b" />
            <span className="orbit orbit-c" />
            <span className="orbit-core">
              <Layers3 size={31} />
            </span>
            {[0, 1, 2, 3, 4, 5].map((n) => (
              <i key={n} style={{ transform: `rotate(${n * 60}deg) translateX(91px)` }} />
            ))}
          </div>
          <div className="absolute bottom-7 right-7 w-[180px] space-y-3 text-[11px] text-white/50">
            <div className="flex justify-between">
              <span>
                <Dot /> Context
              </span>
              <b className="text-white/80">92%</b>
            </div>
            <div className="flex justify-between">
              <span>
                <Dot tone="amber" /> Decisions
              </span>
              <b className="text-white/80">54</b>
            </div>
            <div className="flex justify-between">
              <span>
                <Dot tone="amber" /> Permissions
              </span>
              <b className="text-white/80">128</b>
            </div>
            <div className="flex justify-between">
              <span>Last synced</span>
              <b className="text-white/80">2m ago</b>
            </div>
          </div>
        </Panel>

        <Panel
          className="home-agent-status xl:absolute xl:right-0 xl:top-0 xl:z-10 xl:w-[360px]"
          title="AGENT STATUS"
          action={<Button className="min-h-8 px-3">Manage agents</Button>}
        >
          <div className="divide-y divide-white/[.05]">
            {[
              ["Codex", "Implementing data pipeline", "green"],
              ["Claude Code", "Refactoring auth service", "amber"],
              ["Research Agent", "Analyzing chart libraries", "violet"],
              ["Memory Curator", "Indexing new knowledge", "blue"],
            ].map(([name, task, tone], i) => (
              <div key={name} className="flex items-center gap-3 py-2 first:pt-0">
                <div className={`agent-avatar agent-${tone}`}>
                  <Bot size={17} />
                </div>
                <div className="min-w-0 flex-1">
                  <div className="text-[12px] font-medium text-white/90">{name}</div>
                  <div className="truncate text-[10px] text-white/40">{task}</div>
                </div>
                <Badge tone={i === 2 ? "neutral" : "green"}>{i === 2 ? "Idle" : "Working"}</Badge>
                <Dot />
              </div>
            ))}
          </div>
          <button
            onClick={() => onNavigate("agents")}
            className="mt-2 flex min-h-9 w-full items-center justify-center gap-2 text-[11px] text-amber-400 hover:text-amber-300"
          >
            View all agents <ArrowRight size={13} />
          </button>
        </Panel>
      </div>

      <div className="mt-4 grid gap-4 md:grid-cols-2 xl:grid-cols-4 xl:[&>section]:h-[224px] xl:[&>section]:overflow-hidden">
        <Panel title="ACTIVE PROJECT" action={null}>
          <div className="text-[15px] text-white">SimCharts Rebuild</div>
          <div className="mt-6 flex justify-between text-[10px] text-white/45">
            <span>Progress</span>
            <span>92%</span>
          </div>
          <Progress value={92} className="mt-2" />
          <div className="mt-4 space-y-2 text-[10px] text-white/45">
            <div className="flex justify-between">
              <span>Phase</span>
              <span>Implementation</span>
            </div>
            <div className="flex justify-between">
              <span>Current sprint</span>
              <span>Sprint 7</span>
            </div>
            <div className="flex justify-between">
              <span>Due date</span>
              <span>May 30, 2024</span>
            </div>
          </div>
          <div className="mt-4 border-t border-white/[.05] pt-2 text-center">
            <TextAction>Open project</TextAction>
          </div>
        </Panel>
        <Panel
          title="RECENT DECISIONS"
          action={<span className="text-[10px] text-amber-400">View all</span>}
        >
          <div className="divide-y divide-white/[.05]">
            {decisions.slice(0, 3).map((decision, i) => (
              <div key={decision} className="flex gap-2 py-2.5 first:pt-0">
                <CheckCircle2 size={17} className="mt-0.5 shrink-0 text-amber-400" />
                <div>
                  <div className="text-[10px] leading-4 text-white/75">{decision}</div>
                  <div className="text-[9px] text-white/35">
                    May {12 - i}, {10 + i}:15 AM
                  </div>
                </div>
              </div>
            ))}
          </div>
        </Panel>
        <Panel
          title="OPEN BLOCKERS"
          action={<span className="text-[10px] text-amber-400">View all</span>}
        >
          <div className="divide-y divide-white/[.05]">
            {blockers.map(([label, priority], i) => (
              <div key={label} className="flex items-center gap-2 py-2.5 first:pt-0">
                <div className="flex-1 text-[10px] leading-4 text-white/70">
                  {label}
                  <div className="text-[9px] text-white/35">
                    Assigned to {i === 0 ? "Codex" : "Claude Code"}
                  </div>
                </div>
                <Badge
                  tone={priority === "High" ? "red" : priority === "Medium" ? "amber" : "green"}
                >
                  {priority}
                </Badge>
              </div>
            ))}
          </div>
        </Panel>
        <Panel
          title="CONTEXT HEALTH"
          action={<span className="text-[10px] text-amber-400">View details</span>}
        >
          <div className="flex items-center gap-5">
            <Ring value={92} label="Excellent" />
            <div className="flex-1 space-y-2 text-[10px] text-white/45">
              {[
                ["Completeness", "94%"],
                ["Freshness", "91%"],
                ["Consistency", "93%"],
                ["Coverage", "89%"],
              ].map(([a, b]) => (
                <div key={a} className="flex justify-between">
                  <span>{a}</span>
                  <span className="text-white/70">{b}</span>
                </div>
              ))}
            </div>
          </div>
          <p className="mt-4 text-[10px] leading-5 text-white/40">
            Your context is healthy and up to date.
            <br />
            Last compiled: 12m ago
          </p>
        </Panel>
      </div>

      <div className="mt-4 grid gap-4 xl:grid-cols-[1.35fr_1.05fr_1.05fr]">
        <Panel title="PROACTIVE SUGGESTIONS">
          <div className="space-y-2">
            {[
              ["Compile context before starting Sprint 8", "Compile now"],
              ["Review 3 low-confidence decisions", "Review"],
              ["Add missing API documentation to memory", "Add to memory"],
            ].map(([label, action], i) => (
              <div
                key={label}
                className="flex items-center gap-3 rounded-lg border border-white/[.05] bg-white/[.018] p-3"
              >
                <div className="grid h-8 w-8 place-items-center rounded-lg bg-amber-500/10 text-amber-400">
                  <Sparkles size={15} />
                </div>
                <div className="min-w-0 flex-1">
                  <div className="text-[11px] text-white/80">{label}</div>
                  <div className="text-[9px] text-white/35">
                    {i === 0
                      ? "Fresh context improves agent accuracy."
                      : "Continuum found a useful next step."}
                  </div>
                </div>
                <Badge tone="amber">{action}</Badge>
              </div>
            ))}
          </div>
        </Panel>
        <Panel
          title="RECENT ACTIONS"
          action={<span className="text-[10px] text-amber-400">View all</span>}
        >
          <div className="divide-y divide-white/[.05]">
            {[
              ["Context compiled", "12m ago"],
              ["Decision recorded", "55m ago"],
              ["Agent started", "1h ago"],
              ["Permission granted", "2h ago"],
              ["File added to memory", "3h ago"],
            ].map(([a, b]) => (
              <div key={a} className="flex items-center gap-3 py-2">
                <div className="grid h-7 w-7 place-items-center rounded-full bg-amber-500/10 text-amber-400">
                  <Activity size={13} />
                </div>
                <div className="flex-1">
                  <div className="text-[10px] text-white/75">{a}</div>
                  <div className="text-[9px] text-white/35">SimCharts Rebuild</div>
                </div>
                <span className="text-[9px] text-white/35">{b}</span>
              </div>
            ))}
          </div>
        </Panel>
        <Panel title="QUICK ACTIONS">
          <div className="space-y-2">
            {[
              [Play, "Resume work"],
              [BrainCircuit, "Compile context"],
              [Bot, "Start agent"],
              [ShieldCheck, "Review approvals"],
            ].map(([Icon, label]) => {
              const C = Icon as typeof Play;
              return (
                <button
                  key={label as string}
                  className="flex min-h-11 w-full items-center gap-3 rounded-lg border border-white/[.06] bg-white/[.02] px-3 text-left transition-colors hover:border-amber-500/30 hover:bg-amber-500/[.05]"
                >
                  <C size={15} className="text-amber-400" />
                  <span className="flex-1 text-[11px] text-white/80">{label as string}</span>
                  <ArrowRight size={13} className="text-white/35" />
                </button>
              );
            })}
          </div>
        </Panel>
      </div>
    </>
  );
}

function GraphNode({
  icon,
  label,
  meta,
  className,
}: {
  icon: ReactNode;
  label: string;
  meta: string;
  className: string;
}) {
  return (
    <div className={`graph-node ${className}`}>
      <span>{icon}</span>
      <div>
        <b>{label}</b>
        <small>{meta}</small>
      </div>
    </div>
  );
}

export function ProjectsScreen() {
  return (
    <>
      <PageHeader
        title="SimCharts Rebuild"
        subtitle="Projects / SimCharts Rebuild"
        actions={
          <>
            <Button>
              <Code2 size={14} /> Open in Codex
            </Button>
            <Button>
              <Sparkles size={14} /> Open in Claude Code
            </Button>
            <Button>
              <BrainCircuit size={14} /> Compile context
            </Button>
            <Button variant="primary">
              <Plus size={14} /> Create task
            </Button>
          </>
        }
      />
      <div className="mb-3 grid grid-cols-[1.2fr_.45fr_.8fr_.55fr_2.2fr] gap-4 border-b border-white/[.06] pb-3 text-[10px] text-white/45">
        <div>
          <div>Repository</div>
          <b className="mt-2 block text-[12px] text-white/80">
            <Github size={13} className="mr-2 inline" />
            github.com/vixco/simcharts.net
          </b>
        </div>
        <div>
          <div>Branch</div>
          <b className="mt-2 block text-[12px] text-white/80">main</b>
        </div>
        <div>
          <div>Collaborators</div>
          <b className="mt-2 block text-[12px] text-white/80">● ● ● ● +3</b>
        </div>
        <div>
          <div>Status</div>
          <div className="mt-2">
            <Badge tone="green">● On track</Badge>
          </div>
        </div>
        <div>
          <div>Objective</div>
          <p className="mt-2 text-[12px] leading-5 text-white/70">
            Rebuild SimCharts.net with a modern stack, real-time data, and AI-assisted insights.
          </p>
        </div>
      </div>
      <div className="grid gap-3 xl:grid-cols-[280px_minmax(0,1fr)_320px]">
        <div className="projects-left space-y-3">
          <Panel
            className="project-roadmap"
            title="Roadmap"
            action={<TextAction>View full roadmap</TextAction>}
          >
            <div className="space-y-3 text-[11px]">
              {[
                ["Foundation & Infra", "Done"],
                ["Platform Core", "In progress"],
                ["Real-time Data", "Next"],
                ["Intelligence Layer", "Next"],
                ["Community & Insights", "Planned"],
              ].map(([a, b], i) => (
                <div key={a} className="flex justify-between">
                  <span className="text-white/70">
                    {i === 0 ? "●" : "◆"} {a}
                  </span>
                  <span className={i === 0 ? "text-emerald-400" : "text-white/40"}>{b}</span>
                </div>
              ))}
            </div>
          </Panel>
          <Panel
            className="project-milestone"
            title="Current milestone"
            action={<span className="text-[10px] text-white/40">Ends in 12 days</span>}
          >
            <h3 className="text-[15px] text-white">Platform Core</h3>
            <p className="mt-2 text-[11px] leading-5 text-white/45">
              Bring the core platform online with auth, data services, and dashboards.
            </p>
            <div className="mt-4 flex items-center gap-3">
              <Progress value={62} className="flex-1" />
              <span className="text-[10px] text-white/50">62%</span>
            </div>
          </Panel>
          <Panel className="project-tasks" title="Open tasks" action={<Badge>32</Badge>}>
            <div className="space-y-2 text-[11px] text-white/55">
              <div>
                <Dot tone="amber" /> <span className="ml-2">High priority</span>
                <span className="float-right">8</span>
              </div>
              <div>
                <Dot tone="amber" /> <span className="ml-2">In progress</span>
                <span className="float-right">12</span>
              </div>
              <div>
                <Dot tone="gray" /> <span className="ml-2">Todo</span>
                <span className="float-right">12</span>
              </div>
            </div>
          </Panel>
          <Panel className="project-blockers" title="Blockers" action={<Badge>3</Badge>}>
            {blockers.map(([a, b]) => (
              <div key={a} className="mb-2 flex gap-2 text-[10px]">
                <Dot tone="red" />
                <span className="flex-1 text-white/60">{a}</span>
                <Badge tone={b === "High" ? "red" : b === "Medium" ? "amber" : "green"}>{b}</Badge>
              </div>
            ))}
          </Panel>
        </div>
        <div className="space-y-3">
          <Panel
            title="Project graph"
            action={<Button className="min-h-8">View legend</Button>}
            className="min-h-[360px] overflow-hidden"
          >
            <p className="-mt-3 text-[10px] text-white/35">Visualize how everything connects</p>
            <div className="project-graph mt-1">
              <div className="graph-lines" />
              <div className="graph-core">
                <Box size={27} />
                <b>
                  SimCharts
                  <br />
                  Rebuild
                </b>
              </div>
              <GraphNode
                className="node-decisions"
                icon={<FileText size={16} />}
                label="Decisions"
                meta="24"
              />
              <GraphNode
                className="node-people"
                icon={<Users size={16} />}
                label="People"
                meta="8"
              />
              <GraphNode
                className="node-outcomes"
                icon={<Target size={16} />}
                label="Outcomes"
                meta="5"
              />
              <GraphNode className="node-repos" icon={<Code2 size={16} />} label="Repos" meta="4" />
              <GraphNode
                className="node-blockers"
                icon={<AlertTriangle size={16} />}
                label="Blockers"
                meta="3"
              />
              <GraphNode
                className="node-docs"
                icon={<FileText size={16} />}
                label="Docs"
                meta="18"
              />
              <GraphNode
                className="node-tasks"
                icon={<CheckCircle2 size={16} />}
                label="Tasks"
                meta="32"
              />
            </div>
          </Panel>
          <div className="grid gap-3 md:grid-cols-3 md:[&>section]:h-[145px] md:[&>section]:overflow-hidden">
            <Panel
              title="Linked repositories"
              action={<span className="text-[10px] text-amber-400">View all</span>}
            >
              <List
                lines={[
                  "simcharts.net",
                  "simcharts-api",
                  "simcharts-data-pipeline",
                  "simcharts-infra",
                ]}
                icon={<Github size={12} />}
              />
            </Panel>
            <Panel
              title="Docs"
              action={<span className="text-[10px] text-amber-400">View all</span>}
            >
              <List
                lines={[
                  "Architecture Overview",
                  "Data Model",
                  "API Reference",
                  "Deployment Guide",
                  "Contributing Guide",
                ]}
                icon={<FileText size={12} />}
              />
            </Panel>
            <Panel
              title="Decisions"
              action={<span className="text-[10px] text-amber-400">View all</span>}
            >
              <List lines={decisions} icon={<CircleDot size={12} />} />
            </Panel>
          </div>
        </div>
        <div className="space-y-3">
          <Panel title="Project context" action={<RefreshCw size={14} className="text-white/45" />}>
            <p className="-mt-3 mb-4 text-[10px] text-white/35">Last compiled 2h ago</p>
            <KeyValues
              rows={[
                ["Type", "Web Application"],
                ["Stack", "Next.js, TypeScript, PostgreSQL, Prisma, TailwindCSS"],
                ["Environment", "Vercel (prod) • Railway (data)"],
                ["Codebase", "~48k LOC • 92% TypeScript"],
                ["Test coverage", "47%"],
                ["Activity (7d)", "128 commits • 42 PRs"],
              ]}
            />
          </Panel>
          <Panel
            title="Best next action"
            action={<span className="text-[10px] text-amber-400">Why this?</span>}
          >
            <div className="rounded-lg border border-amber-500/25 bg-amber-500/[.07] p-4">
              <div className="flex items-center gap-3 text-[14px] text-white/90">
                <span className="flex-1">Resolve PostgreSQL performance bottlenecks</span>
                <ArrowRight className="text-amber-400" size={16} />
              </div>
              <div className="mt-3 flex gap-2">
                <Badge tone="amber">High impact</Badge>
                <Badge tone="amber">Unblocks 3 tasks</Badge>
              </div>
            </div>
            <p className="mt-4 text-[11px] leading-5 text-white/50">
              Query performance is degrading key dashboards and blocking real-time features.
            </p>
            <div className="mt-4 flex justify-between">
              <Button variant="primary">Create task</Button>
              <TextAction>View related</TextAction>
            </div>
          </Panel>
        </div>
      </div>
      <div className="projects-footer mt-3 grid gap-3 lg:grid-cols-3">
        <Panel title="Recent commits" action={<TextAction>View all</TextAction>}>
          <List
            lines={[
              "feat(dashboard): add real-time ticker component",
              "fix(api): optimize /markets query with indexes",
              "chore: update dependencies",
              "feat(auth): add magic link sign-in",
            ]}
            icon={<GitCommitHorizontal size={12} />}
          />
        </Panel>
        <Panel title="Failed attempts" action={<TextAction>View all</TextAction>}>
          <List
            lines={[
              "Implement GraphQL layer — Failed tests",
              "Optimize aggregation pipeline — Timeout",
              "Migrate to Turborepo — Build errors",
            ]}
            icon={<XCircle size={12} className="text-red-400" />}
          />
        </Panel>
        <Panel title="Lessons learned" action={<TextAction>View all</TextAction>}>
          <List
            lines={[
              "PostgreSQL window functions improved report queries by 42%.",
              "WebSocket connection pooling reduced disconnects by 80%.",
              "Avoid Prisma $transaction in hot paths.",
              "Cache market metadata for 60s to reduce API calls.",
            ]}
            icon={<Lightbulb size={12} className="text-amber-400" />}
          />
        </Panel>
      </div>
    </>
  );
}

function List({ lines, icon }: { lines: string[]; icon: ReactNode }) {
  return (
    <div className="space-y-2.5">
      {lines.map((line) => (
        <div key={line} className="flex min-w-0 items-center gap-2 text-[10px] text-white/55">
          <span className="shrink-0 text-amber-400">{icon}</span>
          <span className="truncate">{line}</span>
        </div>
      ))}
    </div>
  );
}
function KeyValues({ rows }: { rows: string[][] }) {
  return (
    <div className="space-y-4">
      {rows.map(([a, b]) => (
        <div key={a} className="grid grid-cols-[90px_1fr] gap-2 text-[11px]">
          <span className="text-white/40">{a}</span>
          <span className="leading-4 text-white/65">{b}</span>
        </div>
      ))}
    </div>
  );
}

export function AgentsScreen({
  mode,
  setMode,
}: {
  mode: "handoff" | "launch";
  setMode: (mode: "handoff" | "launch") => void;
}) {
  return mode === "launch" ? (
    <LaunchPad onBack={() => setMode("handoff")} />
  ) : (
    <Handoff onLaunch={() => setMode("launch")} />
  );
}

function Handoff({ onLaunch }: { onLaunch: () => void }) {
  return (
    <>
      <PageHeader
        title="Agent Handoff / Relay"
        subtitle="Continuum transfers work between agents with full context, decisions, and guardrails."
        actions={
          <>
            <Button onClick={onLaunch}>
              <Rocket size={14} /> Context Compiler
            </Button>
            <Badge>Relay ID: relay_7f3b9c</Badge>
          </>
        }
      />
      <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_320px]">
        <div className="space-y-4">
          <Panel className="handoff-hero min-h-[250px] p-8">
            <div className="grid h-full grid-cols-[1fr_1.35fr_1fr] items-center gap-8">
              <AgentCard name="Claude Code" role="Previous agent" issue />
              <div className="relative text-center">
                <div className="handoff-line" />
                <div className="mx-auto grid h-28 w-28 place-items-center rounded-full border border-amber-500/40 bg-amber-500/10 text-amber-400 shadow-[0_0_55px_rgba(245,158,11,.22)]">
                  <Sparkles size={42} />
                </div>
                <h3 className="mt-3 text-[14px] text-white">Continuum</h3>
                <p className="text-[11px] text-white/45">
                  Compiled context, decisions,
                  <br />
                  artifacts, and telemetry
                </p>
                <Badge tone="amber">Context package created</Badge>
              </div>
              <AgentCard name="Codex" role="Next agent" />
            </div>
          </Panel>
          <Panel className="handoff-timeline" title="Handoff timeline">
            <div className="grid grid-cols-6 gap-2">
              {[
                ["Objective", "10:12 AM"],
                ["Files changed", "10:15 AM"],
                ["Tests run", "10:18 AM"],
                ["Errors encountered", "10:21 AM"],
                ["Decisions used", "10:24 AM"],
                ["Next step", "10:25 AM"],
              ].map(([a, b], i) => (
                <div key={a} className="relative pt-10 text-[10px]">
                  <span className="absolute left-0 top-0 grid h-7 w-7 place-items-center rounded-full border border-amber-500/40 bg-amber-500/10 text-amber-400">
                    {i + 1}
                  </span>
                  <b className="block text-white/70">{a}</b>
                  <span className="text-white/35">{b}</span>
                  <p className="mt-4 leading-4 text-white/45">
                    {i === 0
                      ? "Refactor data layer"
                      : i === 1
                        ? "17 files modified"
                        : i === 2
                          ? "42 tests executed"
                          : i === 3
                            ? "7 failing tests"
                            : i === 4
                              ? "4 decisions applied"
                              : "Fix failing tests"}
                  </p>
                </div>
              ))}
            </div>
          </Panel>
          <div className="handoff-summaries grid gap-3 md:grid-cols-3">
            <Panel title="Previous agent">
              <AgentSummary name="Claude Code" status="Partially complete" value={48} />
              <Button className="mt-4 w-full">Open full run</Button>
            </Panel>
            <Panel title="Context package" action={<Badge>Size: 1.2 MB</Badge>}>
              <List
                lines={[
                  "Objective & requirements — 1 item",
                  "Files changed — 17 files",
                  "Tests & results — 42 tests",
                  "Errors & logs — 27 items",
                  "Decisions & rationale — 4 items",
                  "Environment & configs — 3 items",
                ]}
                icon={<FileText size={12} />}
              />
              <Button className="mt-4 w-full">Preview package</Button>
            </Panel>
            <Panel title="Next agent" action={<Badge tone="green">Ready to run</Badge>}>
              <AgentSummary name="Codex" status="Ready" value={76} />
              <Button onClick={onLaunch} className="mt-4 w-full">
                Configure agent
              </Button>
            </Panel>
          </div>
        </div>
        <div className="space-y-3">
          <Panel title="Permissions & guardrails">
            <KeyValues
              rows={[
                ["File system", "Read / Write (workspace)"],
                ["Network", "Allowed (npm, pypi, postgres)"],
                ["Database", "Read / Write (analytics_db)"],
                ["Secrets", "Read (DB_URL, API_KEYS)"],
                ["Policy", "Safe mode • Human approve"],
              ]}
            />
          </Panel>
          <Panel title="Outcome confidence">
            <div className="flex items-center gap-5">
              <Ring value={76} label="High" />
              <List
                lines={[
                  "Similar tasks succeeded",
                  "Sufficient context",
                  "Agent capability match",
                  "Low-risk operations",
                ]}
                icon={<Check size={12} className="text-emerald-400" />}
              />
            </div>
          </Panel>
          <Panel title="Rollback plan">
            <div className="flex gap-3">
              <div className="grid h-11 w-11 place-items-center rounded-lg bg-violet-500/15 text-violet-400">
                <ShieldCheck />
              </div>
              <div>
                <div className="text-[11px] text-white/75">Automatic rollback available</div>
                <p className="text-[10px] leading-4 text-white/40">
                  Database migration reversible
                  <br />
                  Backup snapshot: 10:12 AM
                </p>
              </div>
            </div>
            <Button className="mt-4 w-full">View rollback plan</Button>
          </Panel>
          <Button onClick={onLaunch} variant="primary" className="w-full">
            Continue in Codex <ArrowRight size={14} />
          </Button>
          <Button className="w-full">Approve changes</Button>
          <Button className="w-full">Open full run</Button>
        </div>
      </div>
      <Panel title="All agents" className="mt-4">
        <div className="grid grid-cols-2 gap-3 lg:grid-cols-6">
          {["Claude Code", "Codex", "Gemini 1.5 Pro", "DeepSeek V2", "Llama 3.1 70B"].map(
            (name, i) => (
              <div key={name} className="rounded-lg border border-white/[.07] bg-white/[.02] p-3">
                <div className="flex items-center gap-2">
                  <Bot size={20} className={i < 2 ? "text-amber-400" : "text-white/45"} />
                  <div>
                    <b className="block text-[11px] text-white/75">{name}</b>
                    <span className="text-[9px] text-white/35">
                      {i < 2 ? "Ready" : "Available"}
                    </span>
                  </div>
                </div>
                <Progress value={i === 0 ? 48 : i === 1 ? 76 : 0} className="mt-5" />
              </div>
            )
          )}
          <button className="min-h-[86px] rounded-lg border border-dashed border-white/10 text-[11px] text-white/50 hover:border-amber-500/40 hover:text-amber-400">
            <Plus className="mr-2 inline" size={14} />
            Add agent
          </button>
        </div>
      </Panel>
    </>
  );
}

function AgentCard({ name, role, issue }: { name: string; role: string; issue?: boolean }) {
  return (
    <div className="rounded-xl border border-white/[.09] bg-black/20 p-5">
      <div className="text-[10px] text-white/45">{role}</div>
      <div className="mt-1 flex items-center gap-2 text-[15px] text-white">
        <Bot className="text-amber-400" /> {name}
      </div>
      <div className="my-4 h-px bg-white/[.06]" />
      <div className="text-[10px] text-white/40">
        {issue ? "Attempted task" : "Best suited for"}
      </div>
      <p className="mt-1 text-[11px] leading-4 text-white/70">
        {issue
          ? "Refactor data layer to PostgreSQL + add analytics store"
          : "Fix failing tests, finalize migration, optimize queries"}
      </p>
      <div className={`mt-4 text-[10px] ${issue ? "text-red-400" : "text-emerald-400"}`}>
        ● {issue ? "Encountered issues" : "Ready to continue"}
      </div>
    </div>
  );
}
function AgentSummary({ name, status, value }: { name: string; status: string; value: number }) {
  return (
    <div>
      <div className="flex items-center gap-2 text-[14px] text-white">
        <Bot size={22} className="text-amber-400" />
        {name}
      </div>
      <KeyValues
        rows={[
          ["Model", name === "Codex" ? "GPT-4o (Codex)" : "Claude 3.7 Sonnet"],
          ["Duration", name === "Codex" ? "18–25m" : "27m 14s"],
          ["Outcome", status],
          ["Guardrails", name === "Codex" ? "Strict" : "—"],
        ]}
      />
      <div className="mt-3 flex items-center gap-3">
        <Progress value={value} className="flex-1" />
        <span className="text-[10px] text-white/40">{value}%</span>
      </div>
    </div>
  );
}

function LaunchPad({ onBack }: { onBack: () => void }) {
  const stages = [
    ["Detect Project", "SimCharts Rebuild"],
    ["Retrieve Decisions", "54 relevant"],
    ["Collect Files", "47 files"],
    ["Summarize Blockers", "3 blockers"],
    ["Apply Permissions", "Read + Edit"],
    ["Compile Package", "9.2 MB"],
    ["Launch Agent", "Ready"],
  ];
  return (
    <>
      <PageHeader
        title="Context Compiler / Launch Pad"
        subtitle="Continuum prepares the right context before any agent starts."
        actions={<Button onClick={onBack}>Back to handoff</Button>}
      />
      <div className="mb-4 grid grid-cols-7 gap-2">
        {stages.map(([a, b], i) => (
          <div
            key={a}
            className={`rounded-lg border p-3 ${i === 6 ? "border-amber-500/40 bg-amber-500/[.08]" : "border-white/[.07] bg-white/[.02]"}`}
          >
            <div className="text-[10px] text-white/70">{a}</div>
            <div className="mt-1 text-[9px] text-white/35">{b}</div>
            <Check size={14} className="mt-3 text-emerald-400" />
          </div>
        ))}
      </div>
      <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_320px]">
        <div className="space-y-4">
          <Panel
            className="launch-context-panel"
            title="Context Package: SimCharts Rebuild"
            action={<Badge tone="amber">Ready</Badge>}
          >
            <div className="launch-context-grid grid gap-3 md:grid-cols-2">
              <ContextBlock
                title="Goal"
                lines={[
                  "Adopt PostgreSQL for analytics store in SimCharts Rebuild to improve query performance, reduce costs, and increase scalability.",
                ]}
              />
              <ContextBlock title="Decisions (54)" lines={decisions.slice(0, 3)} />
              <ContextBlock
                title="Relevant files (47)"
                lines={[
                  "src/db/postgres/client.ts",
                  "src/db/postgres/migrations/001_initial.sql",
                  "src/services/analytics/metrics.repository.ts",
                  "src/api/analytics/routes.ts",
                  "infra/terraform/rds.tf",
                ]}
              />
              <ContextBlock
                title="Open tasks (31)"
                lines={[
                  "Implement data migration job",
                  "Backfill historical analytics data",
                  "Update analytics queries to Postgres dialect",
                ]}
              />
              <ContextBlock
                title="Constraints"
                lines={[
                  "Maintain API compatibility",
                  "No downtime during migration",
                  "Data consistency must be preserved",
                  "Follow internal security guidelines",
                ]}
              />
              <ContextBlock
                title="Known failures (7)"
                lines={[
                  "PSQL connection timeout under high load",
                  "Long running queries causing lock contention",
                  "Index bloat on analytics_events table",
                ]}
              />
            </div>
            <div className="mt-3 rounded-lg border border-white/[.06] p-3 text-[10px] text-white/50">
              <b className="mr-4 text-amber-400">Allowed tools</b> Read Files　 Search Code　 Edit
              Files　 Run Tests　 Git　 SQL (Read)　 Shell (Restricted)
            </div>
          </Panel>
          <Panel
            className="launch-history"
            title="Previous Launches"
            action={<Button className="min-h-8">View All Launches</Button>}
          >
            <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_150px]">
              <div className="continuum-table">
                <div className="table-head table-row">
                  <span>Launch</span>
                  <span>Agent</span>
                  <span>Project</span>
                  <span>Started</span>
                  <span>Duration</span>
                  <span>Success</span>
                  <span>Result</span>
                </div>
                {[1287, 1286, 1285, 1284, 1283].map((id, i) => (
                  <div className="table-row" key={id}>
                    <span className="flex items-center gap-2">
                      <Rocket size={12} className="text-amber-400" /> Launch #{id}
                    </span>
                    <span>{i % 2 ? "Claude Code" : "Codex"}</span>
                    <span>SimCharts Rebuild</span>
                    <span>{i ? "Yesterday 3:42 PM" : "Today 10:15 AM"}</span>
                    <span>{24 + i * 4}m 18s</span>
                    <span>{98 - i * 2}%</span>
                    <span className="text-emerald-400">● Completed</span>
                  </div>
                ))}
              </div>
              <div className="flex flex-col items-center justify-center border-l border-white/[.06] pl-4 text-center">
                <Ring value={94} />
                <div className="mt-2 text-[10px] text-white/50">
                  28 of 30 launches
                  <br />
                  completed successfully
                </div>
              </div>
            </div>
          </Panel>
        </div>
        <Panel title="Launch Options">
          <div className="space-y-2">
            {["Codex", "Claude Code", "Local Agent"].map((a, i) => (
              <button
                key={a}
                className={`flex min-h-12 w-full items-center gap-3 rounded-lg border px-3 text-left ${i === 0 ? "border-amber-500/50 bg-amber-500/[.05]" : "border-white/[.07]"}`}
              >
                <CircleDot size={15} className={i === 0 ? "text-amber-400" : "text-white/30"} />
                <div>
                  <b className="block text-[11px] text-white/80">{a}</b>
                  <span className="text-[9px] text-white/40">
                    {i === 0
                      ? "Best for full-stack engineering tasks"
                      : i === 1
                        ? "Best for complex reasoning & refactors"
                        : "Runs locally with your compute"}
                  </span>
                </div>
              </button>
            ))}
          </div>
          <h3 className="mt-7 text-[11px] text-white/60">Autonomy Level</h3>
          <div className="mt-2 grid grid-cols-3 gap-2">
            {["Low", "Medium", "High"].map((a) => (
              <button
                key={a}
                className={`min-h-14 rounded-lg border text-[10px] ${a === "Medium" ? "border-amber-500/50 bg-amber-500/[.07] text-white" : "border-white/[.07] text-white/45"}`}
              >
                {a}
              </button>
            ))}
          </div>
          <h3 className="mt-7 text-[11px] text-white/60">Approval Mode</h3>
          <div className="mt-2 grid grid-cols-3 gap-2">
            {["Suggest", "Auto-approve", "Strict"].map((a) => (
              <button
                key={a}
                className={`min-h-14 rounded-lg border text-[10px] ${a === "Auto-approve" ? "border-amber-500/50 bg-amber-500/[.07] text-white" : "border-white/[.07] text-white/45"}`}
              >
                {a}
              </button>
            ))}
          </div>
          <label className="mt-7 block text-[11px] text-white/60">
            Additional Instructions (optional)
            <textarea
              className="mt-2 h-24 w-full resize-none rounded-lg border border-white/[.08] bg-black/20 p-3 text-[11px] text-white outline-none focus:border-amber-500/50"
              placeholder="Add specific instructions for the agent..."
            />
          </label>
          <Button variant="primary" className="mt-5 w-full">
            <Rocket size={15} /> Launch Agent
          </Button>
        </Panel>
      </div>
    </>
  );
}
function ContextBlock({ title, lines }: { title: string; lines: string[] }) {
  return (
    <div className="context-block overflow-hidden rounded-lg border border-white/[.07] bg-black/10 p-3">
      <h3 className="mb-3 text-[11px] font-medium text-amber-400">{title}</h3>
      <div className="space-y-2">
        {lines.map((line) => (
          <div key={line} className="text-[10px] leading-4 text-white/60">
            {line}
          </div>
        ))}
      </div>
    </div>
  );
}

const eventRows = [
  ["Decision", "Adopt PostgreSQL for analytics store", "92% confidence", "amber"],
  ["Agent run", "Data Quality Agent completed", "Success", "green"],
  [
    "Permission approval",
    "Agent “Data Quality Agent” granted access to Redshift",
    "Approved",
    "violet",
  ],
  ["Commit", "feat(analytics): initial schema for metrics & events", "a1b2c3d", "blue"],
  ["Context compilation", "Architecture overview compiled", "v3", "amber"],
  ["Deployment attempt", "Deploy to staging", "Failed", "red"],
  ["Decision", "Normalize event schema for long-term retention", "92% confidence", "amber"],
  ["Agent run", "SimCharts ETL Agent completed", "Success", "green"],
];

const timelineToneClasses: Record<string, string> = {
  amber: "bg-amber-500/10 text-amber-400",
  blue: "bg-blue-500/10 text-blue-400",
  green: "bg-emerald-500/10 text-emerald-400",
  red: "bg-red-500/10 text-red-400",
  violet: "bg-violet-500/10 text-violet-400",
};

export function TimelineScreen() {
  return (
    <>
      <PageHeader
        title="Timeline / Audit Trail"
        subtitle="A chronological record of key events across projects, agents, and system activity."
      />
      <div className="grid gap-3 lg:grid-cols-4">
        <StatCard icon={<Activity />} label="Events today" value="128" meta="↑ 18% vs yesterday" />
        <StatCard
          icon={<ShieldCheck />}
          label="Approvals pending"
          value="7"
          meta="Requires your review"
        />
        <StatCard
          icon={<CheckCircle2 />}
          label="Successful agent runs"
          value="23"
          meta="↑ 8% vs yesterday"
          tone="green"
        />
        <StatCard
          icon={<XCircle />}
          label="Failed runs"
          value="2"
          meta="↓ 1 vs yesterday"
          tone="red"
        />
      </div>
      <div className="mt-3 grid gap-3 xl:grid-cols-[minmax(0,1fr)_310px]">
        <div>
          <div className="mb-3 flex flex-wrap gap-2">
            {[
              "All events",
              "Decisions",
              "Agent runs",
              "Permissions",
              "Commits",
              "Context compilations",
              "Deployments",
            ].map((x, i) => (
              <Button
                key={x}
                className={`min-h-8 px-3 ${i === 0 ? "border-amber-500/50 bg-amber-500/[.08] text-amber-400" : ""}`}
              >
                {x}
              </Button>
            ))}
            <label className="continuum-search ml-auto">
              <input aria-label="Search timeline" placeholder="Search timeline..." />
              <Search size={13} />
            </label>
          </div>
          <Panel className="overflow-hidden p-0">
            <div className="divide-y divide-white/[.05]">
              {eventRows.map(([type, title, badge, tone], i) => (
                <button
                  key={title}
                  className={`grid min-h-[59px] w-full grid-cols-[100px_26px_1fr_auto] items-center gap-3 px-4 text-left hover:bg-white/[.02] ${i === 0 ? "border border-amber-500/50 bg-amber-500/[.04]" : ""}`}
                >
                  <span className="text-[10px] leading-4 text-white/45">
                    {10 - i}:15:24 AM
                    <br />
                    May 19, 2024
                  </span>
                  <span
                    className={`grid h-7 w-7 place-items-center rounded-full ${timelineToneClasses[tone] ?? timelineToneClasses.amber}`}
                  >
                    <Activity size={13} />
                  </span>
                  <span>
                    <b className="mr-3 text-[10px] font-medium text-amber-400">{type}</b>
                    <span className="text-[12px] text-white/75">{title}</span>
                    <small className="mt-1 block text-[9px] text-white/35">
                      Project: SimCharts Rebuild　•　By Toshan Soekar
                    </small>
                  </span>
                  <Badge
                    tone={
                      tone === "green"
                        ? "green"
                        : tone === "red"
                          ? "red"
                          : tone === "violet"
                            ? "violet"
                            : tone === "blue"
                              ? "blue"
                              : "amber"
                    }
                  >
                    {badge}
                  </Badge>
                </button>
              ))}
            </div>
          </Panel>
          <div className="mt-3 grid gap-3 md:grid-cols-2">
            <Panel title="Key moments" action={<TextAction>View all</TextAction>}>
              <List
                lines={[
                  "Major decision recorded: PostgreSQL for analytics store",
                  "Data Quality Agent success improved data freshness by 18%",
                  "New analytics schema committed to main",
                  "Staging deployment failed due to migration error",
                ]}
                icon={<Goal size={12} className="text-amber-400" />}
              />
            </Panel>
            <Panel title="Lessons learned" action={<TextAction>View all</TextAction>}>
              <List
                lines={[
                  "Validate migration scripts in isolated environment before staging",
                  "Data quality checks prevent downstream analytics drift",
                  "Clear decision records reduce rework and alignment time",
                  "Automate schema documentation with each commit",
                ]}
                icon={<Check size={12} className="text-emerald-400" />}
              />
            </Panel>
          </div>
        </div>
        <Panel title="Event details" action={<span className="text-white/40">×</span>}>
          <Badge tone="amber">Decision</Badge>
          <h2 className="mt-4 text-[17px] text-white">Adopt PostgreSQL for analytics store</h2>
          <Badge>92% confidence</Badge>
          <h3 className="mt-6 text-[10px] text-white/45">Overview</h3>
          <p className="mt-2 text-[11px] leading-5 text-white/55">
            Decision to adopt PostgreSQL as the primary analytics datastore for the SimCharts
            platform based on performance, extensibility, and cost efficiency.
          </p>
          <div className="mt-6">
            <KeyValues
              rows={[
                ["Source", "Decision recorded"],
                ["Time", "May 19, 2024 10:15:24 AM"],
                ["Project", "SimCharts Rebuild"],
                ["Recorded by", "Toshan Soekar"],
                ["Confidence", "92%"],
                ["Sensitivity", "Internal"],
                ["Related items", "12 connections"],
              ]}
            />
          </div>
          <h3 className="mt-7 text-[11px] text-white/65">Related items (3)</h3>
          <div className="mt-3">
            <List
              lines={[
                "Project brief: Data platform modernization",
                "Analytics store comparison",
                "Team discussion #analytics",
              ]}
              icon={<FileText size={12} />}
            />
          </div>
          <h3 className="mt-7 text-[11px] text-white/65">Linked evidence (2)</h3>
          <div className="mt-3">
            <List
              lines={["PostgreSQL benchmark results", "Cost analysis - Q2 2024"]}
              icon={<Database size={12} className="text-emerald-400" />}
            />
          </div>
          <div className="mt-8 flex gap-2">
            <Button className="flex-1">Open in project</Button>
            <Button>Export</Button>
          </div>
        </Panel>
      </div>
    </>
  );
}

export function SettingsScreen({
  autoUpdateEnabled,
  onAutoUpdateChange,
  updateState,
  onCheckForUpdates,
  onInstallUpdate,
}: {
  autoUpdateEnabled: boolean;
  onAutoUpdateChange: (enabled: boolean) => void;
  updateState: {
    phase: string;
    update: UpdateInfo | null;
    message: string | null;
    progress: number | null;
  };
  onCheckForUpdates: () => void;
  onInstallUpdate: () => void;
}) {
  const tools = [
    ["Codex", "Full access", "2m ago"],
    ["Claude Code", "Full access", "3m ago"],
    ["Local Models", "Local only", "1m ago"],
    ["GitHub Repos", "Read & write", "5m ago"],
    ["Terminal Sessions", "Read & write", "2m ago"],
    ["Filesystem", "Read only", "1m ago"],
    ["MCP Tools", "Custom", "4m ago"],
  ];
  return (
    <>
      <PageHeader
        title="Integrations & Models"
        subtitle="Connect tools, configure models, and control how Continuum interacts with your ecosystem."
        actions={
          <label className="continuum-search hidden lg:flex">
            <Search size={14} />
            <input
              aria-label="Search integrations"
              placeholder="Search integrations, models, tools..."
            />
          </label>
        }
      />
      <div className="mb-4">
        <ResourcePanel />
      </div>
      <div className="mb-4 flex gap-7 border-b border-white/[.07] px-3 text-[11px]">
        {["Integrations", "Models", "Routing", "Adapters", "Permissions", "Diagnostics"].map(
          (x, i) => (
            <button
              key={x}
              className={`min-h-10 border-b-2 ${i === 0 ? "border-amber-400 text-amber-400" : "border-transparent text-white/55"}`}
            >
              {x}
            </button>
          )
        )}
      </div>
      <Panel className="mb-3" title="Continuum updates">
        <div className="flex flex-wrap items-center gap-4">
          <div className="min-w-[240px] flex-1">
            <div className="text-[12px] text-white/80">Keep Continuum current</div>
            <p className="mt-1 text-[10px] leading-4 text-white/40">
              Updates are checked securely at startup and installed from signed release artifacts.
            </p>
          </div>
          <label className="flex cursor-pointer items-center gap-2 text-[11px] text-white/70">
            <input
              type="checkbox"
              checked={autoUpdateEnabled}
              onChange={(event) => onAutoUpdateChange(event.target.checked)}
              className="h-4 w-4 accent-amber-400"
            />
            Install updates automatically
          </label>
          <Button onClick={onCheckForUpdates}>
            <RefreshCw
              size={13}
              className={updateState.phase === "checking" ? "animate-spin" : ""}
            />
            Check for updates
          </Button>
        </div>
        <div className="mt-3 border-t border-white/[.06] pt-3 text-[10px] text-white/45">
          {updateState.phase === "checking" && "Checking for updates…"}
          {updateState.phase === "current" && "You are up to date."}
          {updateState.phase === "available" && updateState.update && (
            <span className="flex flex-wrap items-center gap-3 text-amber-300">
              Update v{updateState.update.version} is available.
              <Button className="min-h-8 px-3" onClick={onInstallUpdate}>
                Install now
              </Button>
            </span>
          )}
          {updateState.phase === "downloading" && (
            <>
              Installing update{updateState.progress !== null ? ` (${updateState.progress}%)` : ""}…
            </>
          )}
          {updateState.phase === "error" && (
            <span className="text-red-300">{updateState.message ?? "Update check failed."}</span>
          )}
        </div>
      </Panel>
      <div className="grid gap-3 xl:grid-cols-[290px_minmax(0,1fr)_390px]">
        <div className="space-y-3">
          <Panel title="Connected tools" action={<Badge>12</Badge>}>
            <div className="space-y-2">
              {tools.map(([a, b, c], i) => (
                <button
                  key={a}
                  className={`flex min-h-12 w-full items-center gap-3 rounded-lg border px-3 text-left ${i === 0 ? "border-amber-500/30 bg-amber-500/[.05]" : "border-white/[.06] bg-white/[.015]"}`}
                >
                  <div className="grid h-8 w-8 place-items-center rounded-lg border border-amber-500/20 text-amber-400">
                    {i < 2 ? (
                      <Bot size={16} />
                    ) : i === 3 ? (
                      <Github size={16} />
                    ) : i === 4 ? (
                      <TerminalSquare size={16} />
                    ) : (
                      <Box size={16} />
                    )}
                  </div>
                  <div className="flex-1">
                    <b className="block text-[11px] text-white/75">{a}</b>
                    <span className="text-[9px] text-emerald-400">○ Connected</span>
                  </div>
                  <div className="text-right text-[8px] text-white/35">
                    {b}
                    <br />
                    {c}　<Dot />
                  </div>
                </button>
              ))}
            </div>
            <div className="mt-2 text-center">
              <TextAction>View all integrations</TextAction>
            </div>
          </Panel>
          <Panel title="System diagnostics">
            <KeyValues
              rows={[
                ["Service health", "All systems operational"],
                ["Background jobs", "8 of 8 running"],
                ["Queue depth", "23 tasks pending"],
                ["Rate limits", "Normal"],
              ]}
            />
            <div className="mt-3 text-center">
              <TextAction>Open diagnostics</TextAction>
            </div>
          </Panel>
        </div>
        <div className="space-y-3">
          <Panel
            title="Connected models"
            action={<Button className="min-h-8">Manage models</Button>}
          >
            <p className="-mt-3 text-[10px] text-white/35">
              Route tasks to the best model for the job.
            </p>
            <div className="mt-4 grid grid-cols-4 gap-2">
              {[
                ["GPT-4o", "Best for complex reasoning and coding."],
                ["Claude 3.5 Sonnet", "Best for long context and analysis."],
                ["Claude 3 Opus", "Best for deep research and synthesis."],
                ["Local (Ollama)", "Private & offline local inference."],
              ].map(([a, b], i) => (
                <div
                  key={a}
                  className="rounded-lg border border-amber-500/15 bg-amber-500/[.035] p-3"
                >
                  <Bot size={20} className="text-amber-400" />
                  <b className="mt-3 block text-[11px] text-white/80">{a}</b>
                  <p className="mt-2 min-h-10 text-[9px] leading-4 text-white/40">{b}</p>
                  <div className="mt-3 flex justify-between text-[8px] text-white/35">
                    <span>
                      <Dot /> Online
                    </span>
                    <span>{420 + i * 100}ms</span>
                  </div>
                </div>
              ))}
            </div>
          </Panel>
          <Panel
            title="Agent adapters & tool permissions"
            action={<Button className="min-h-8">Manage adapters</Button>}
          >
            <div className="continuum-table">
              <div className="table-head table-row">
                <span>Agent</span>
                <span>Adapter</span>
                <span>Allowed tools</span>
                <span>Permissions</span>
                <span>Last used</span>
                <span>Status</span>
                <span />
              </div>
              {[
                [
                  "Data Quality Agent",
                  "codex-adapter",
                  "7 tools",
                  "Full access",
                  "2m ago",
                  "Active",
                ],
                ["Research Agent", "claude-adapter", "6 tools", "Read & write", "5m ago", "Active"],
                ["Infra Agent", "local-adapter", "5 tools", "Read only", "12m ago", "Active"],
                ["Ops Agent", "mcp-adapter", "9 tools", "Custom", "1h ago", "Idle"],
              ].map((row) => (
                <div className="table-row" key={row[0]}>
                  {row.map((x, i) => (
                    <span key={i} className={i === 5 ? "text-emerald-400" : ""}>
                      {x}
                    </span>
                  ))}
                  <span>•••</span>
                </div>
              ))}
            </div>
            <div className="mt-3 text-center">
              <TextAction>View all agents</TextAction>
            </div>
          </Panel>
          <Panel title="Storage & memory">
            <div className="grid grid-cols-2 gap-8">
              <div className="space-y-4">
                {[
                  ["Local memory database", "24.7 GB", 36],
                  ["Indexed projects", "128 projects", 64],
                  ["Vector index size", "18.3 GB", 28],
                  ["Attachments & assets", "6.2 GB", 41],
                ].map(([a, b, c]) => (
                  <div key={a as string}>
                    <div className="flex justify-between text-[10px] text-white/50">
                      <span>{a as string}</span>
                      <span>{b as string}</span>
                    </div>
                    <Progress value={c as number} className="mt-2" />
                  </div>
                ))}
              </div>
              <div>
                <KeyValues
                  rows={[
                    ["Auto sync", "Every 5 minutes"],
                    ["Last sync", "2 minutes ago"],
                    ["Next sync", "In 2 minutes"],
                    ["Sync status", "Up to date"],
                  ]}
                />
                <Button className="mt-4 w-full">
                  <RefreshCw size={13} />
                  Sync now
                </Button>
              </div>
            </div>
          </Panel>
        </div>
        <Panel title="Codex" action={<span className="text-white/40">×</span>}>
          <div className="-mt-2 flex items-center gap-2 text-[11px] text-emerald-400">
            <Dot /> Connected
          </div>
          <div className="mt-5 flex gap-7 border-b border-white/[.06] text-[10px]">
            {["Overview", "Configuration", "Scopes", "Safety", "Logs"].map((x, i) => (
              <button
                key={x}
                className={`min-h-9 border-b-2 ${i === 0 ? "border-amber-400 text-amber-400" : "border-transparent text-white/45"}`}
              >
                {x}
              </button>
            ))}
          </div>
          <p className="mt-5 text-[11px] leading-5 text-white/50">
            Codex is connected and ready to execute coding tasks, create PRs, and manage
            repositories.
          </p>
          <div className="mt-4 grid grid-cols-2 gap-px overflow-hidden rounded-lg border border-white/[.06] bg-white/[.06]">
            {[
              ["Connection status", "Connected"],
              ["Permissions", "Full access"],
              ["Last sync", "2 minutes ago"],
              ["Health", "Excellent"],
            ].map(([a, b]) => (
              <div key={a} className="bg-[#111210] p-4">
                <span className="text-[9px] text-white/35">{a}</span>
                <b className="mt-2 block text-[10px] text-emerald-400">● {b}</b>
              </div>
            ))}
          </div>
          <Panel
            title="Scopes"
            action={<Badge tone="amber">Edit</Badge>}
            className="mt-3 bg-black/10"
          >
            <List
              lines={[
                "Repositories — All accessible repositories",
                "Pull requests — Read, write, and merge",
                "Issues — Read and comment",
                "Workflows — Read and trigger",
                "Secrets — Read selected secrets",
              ]}
              icon={<CheckCircle2 size={12} className="text-emerald-400" />}
            />
          </Panel>
          <Panel
            title="Safety & controls"
            action={<Badge tone="amber">Edit</Badge>}
            className="mt-3 bg-black/10"
          >
            <List
              lines={[
                "Human approval — Required for high-risk actions",
                "Allowed actions — Code, PRs, Issues, Workflows",
                "Blocked actions — Delete repos, Force push",
                "Data access — Repository metadata and code",
                "Audit logging — All actions are logged",
              ]}
              icon={<ShieldCheck size={12} className="text-amber-400" />}
            />
          </Panel>
          <div className="mt-4 grid grid-cols-2 gap-2">
            <Button>Test connection</Button>
            <Button variant="danger">Disconnect</Button>
          </div>
        </Panel>
      </div>
    </>
  );
}

export function MemoryScreen() {
  return (
    <>
      <PageHeader
        title="Memory / Verified Context"
        subtitle="Inspect what Continuum knows, where it came from, and whether it is still valid."
        actions={
          <Button variant="primary">
            <Plus size={14} /> Add memory
          </Button>
        }
      />
      <div className="grid gap-3 lg:grid-cols-4">
        <StatCard
          icon={<Database />}
          label="Verified memories"
          value="1,284"
          meta="92% with linked evidence"
        />
        <StatCard
          icon={<BrainCircuit />}
          label="Inferred"
          value="87"
          meta="Awaiting confirmation"
        />
        <StatCard
          icon={<AlertTriangle />}
          label="Disputed"
          value="6"
          meta="Needs your review"
          tone="red"
        />
        <StatCard icon={<RefreshCw />} label="Superseded" value="143" meta="Retained in history" />
      </div>
      <div className="mt-4 grid gap-4 xl:grid-cols-[280px_minmax(0,1fr)_340px]">
        <Panel title="Memory map">
          <div className="space-y-2">
            {[
              ["All memory", "1,520"],
              ["Projects", "682"],
              ["Decisions", "241"],
              ["Conventions", "128"],
              ["People", "48"],
              ["Outcomes", "312"],
              ["Permissions", "109"],
            ].map(([a, b], i) => (
              <button
                key={a}
                className={`flex min-h-10 w-full items-center rounded-lg px-3 text-[11px] ${i === 0 ? "bg-amber-500/10 text-amber-400" : "text-white/55 hover:bg-white/[.03]"}`}
              >
                <Database size={14} className="mr-3" />
                <span className="flex-1 text-left">{a}</span>
                <Badge>{b}</Badge>
              </button>
            ))}
          </div>
        </Panel>
        <Panel
          title="Recent verified memories"
          action={
            <label className="continuum-search">
              <Search size={13} />
              <input aria-label="Search memory" placeholder="Search memory..." />
            </label>
          }
        >
          <div className="divide-y divide-white/[.05]">
            {decisions
              .concat([
                "Use strict TypeScript across frontend",
                "Never deploy without approval",
                "PostgreSQL migration attempt failed",
              ])
              .map((a, i) => (
                <button key={a} className="flex min-h-16 w-full items-center gap-3 py-3 text-left">
                  <div className="grid h-8 w-8 place-items-center rounded-full bg-amber-500/10 text-amber-400">
                    <FileText size={14} />
                  </div>
                  <div className="flex-1">
                    <div className="text-[11px] text-white/75">{a}</div>
                    <div className="mt-1 text-[9px] text-white/35">
                      SimCharts Rebuild •{" "}
                      {i < 4 ? "Confirmed by Toshan" : "Observed from repository"}
                    </div>
                  </div>
                  <Badge tone={i === 6 ? "red" : i < 4 ? "green" : "amber"}>
                    {i === 6 ? "Disputed" : i < 4 ? "Confirmed" : "Observed"}
                  </Badge>
                  <span className="text-[10px] text-white/35">{98 - i}%</span>
                </button>
              ))}
          </div>
        </Panel>
        <Panel title="Memory details">
          <Badge tone="green">Confirmed</Badge>
          <h2 className="mt-4 text-[16px] text-white">Adopt PostgreSQL for analytics store</h2>
          <p className="mt-4 text-[11px] leading-5 text-white/50">
            Use PostgreSQL as the primary analytics datastore for SimCharts.
          </p>
          <div className="mt-6">
            <KeyValues
              rows={[
                ["Source", "Decision recorded"],
                ["Evidence", "3 linked artifacts"],
                ["Confidence", "98%"],
                ["Valid from", "May 12, 2024"],
                ["Sensitivity", "Internal"],
                ["Supersedes", "DEC-18"],
                ["Confirmed by", "Toshan Soekar"],
              ]}
            />
          </div>
          <div className="mt-6 grid grid-cols-2 gap-2">
            <Button>View evidence</Button>
            <Button>Edit memory</Button>
          </div>
        </Panel>
      </div>
    </>
  );
}

export function PermissionsScreen() {
  return (
    <>
      <PageHeader
        title="Permissions / Guardrails"
        subtitle="Decide what every agent can read, change, or execute before a tool runs."
        actions={
          <Button variant="primary">
            <Plus size={14} /> New policy
          </Button>
        }
      />
      <div className="grid gap-3 lg:grid-cols-4">
        <StatCard
          icon={<ShieldCheck />}
          label="Allowed today"
          value="128"
          meta="Within approved policy"
          tone="green"
        />
        <StatCard
          icon={<Clock3 />}
          label="Awaiting approval"
          value="7"
          meta="Requires your review"
        />
        <StatCard
          icon={<XCircle />}
          label="Blocked"
          value="12"
          meta="Unsafe or out of scope"
          tone="red"
        />
        <StatCard icon={<KeyRound />} label="Active policies" value="18" meta="Across 4 projects" />
      </div>
      <div className="mt-4 grid gap-4 xl:grid-cols-[minmax(0,1fr)_360px]">
        <Panel
          title="Project policies"
          action={<Button className="min-h-8">Manage policies</Button>}
        >
          <div className="continuum-table">
            <div className="table-head table-row">
              <span>Agent</span>
              <span>Project</span>
              <span>Filesystem</span>
              <span>Network</span>
              <span>Database</span>
              <span>Approval</span>
              <span>Status</span>
            </div>
            {[
              [
                "Codex",
                "SimCharts Rebuild",
                "Workspace R/W",
                "npm, GitHub",
                "analytics_db R/W",
                "High-risk only",
                "Active",
              ],
              [
                "Claude Code",
                "SimCharts Rebuild",
                "Workspace R/W",
                "npm, docs",
                "analytics_db Read",
                "Every write",
                "Active",
              ],
              [
                "Research Agent",
                "Global",
                "Read only",
                "Public web",
                "Blocked",
                "Not required",
                "Active",
              ],
              [
                "Memory Curator",
                "Global",
                "Memory only",
                "Blocked",
                "memory_db R/W",
                "Automatic",
                "Active",
              ],
            ].map((row) => (
              <div className="table-row" key={row[0]}>
                {row.map((x, i) => (
                  <span key={x} className={i === 6 ? "text-emerald-400" : ""}>
                    {x}
                  </span>
                ))}
              </div>
            ))}
          </div>
        </Panel>
        <Panel title="Approval queue" action={<Badge tone="amber">7 pending</Badge>}>
          {[
            ["Claude Code", "Write /src/api/auth.ts"],
            ["Codex", "Run database migration"],
            ["Ops Agent", "Deploy to staging"],
          ].map(([a, b], i) => (
            <div key={b} className="mb-3 rounded-lg border border-white/[.07] bg-white/[.02] p-3">
              <div className="flex items-center gap-2">
                <Bot size={14} className="text-amber-400" />
                <b className="text-[11px] text-white/75">{a}</b>
                <Badge tone={i === 1 ? "red" : "amber"}>{i === 1 ? "High" : "Medium"}</Badge>
              </div>
              <p className="mt-2 text-[10px] text-white/50">{b}</p>
              <div className="mt-3 grid grid-cols-2 gap-2">
                <Button className="min-h-8">Deny</Button>
                <Button variant="primary" className="min-h-8">
                  Approve
                </Button>
              </div>
            </div>
          ))}
        </Panel>
      </div>
      <Panel title="Enforcement flow" className="mt-4">
        <div className="grid grid-cols-5 items-center gap-3">
          {[
            ["1", "Agent requests action"],
            ["2", "Continuum resolves policy"],
            ["3", "Allow / Ask / Deny"],
            ["4", "Tool executes"],
            ["5", "Evidence recorded"],
          ].map(([n, a], i) => (
            <div key={a} className="relative rounded-lg border border-white/[.07] p-4 text-center">
              <span className="mx-auto grid h-7 w-7 place-items-center rounded-full bg-amber-500/10 text-[11px] text-amber-400">
                {n}
              </span>
              <div className="mt-3 text-[10px] text-white/60">{a}</div>
              {i < 4 && (
                <ArrowRight size={14} className="absolute -right-5 top-1/2 text-amber-400" />
              )}
            </div>
          ))}
        </div>
      </Panel>
    </>
  );
}
