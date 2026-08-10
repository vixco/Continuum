# Connected memory and world-model contract

Continuum already has the right storage primitive for connected memory: markdown notes are authoritative, while SQLite/FTS is a disposable projection containing resolved edges. This workstream therefore does **not** add a dedicated graph database.

The connected-memory layer adds deterministic policy above that existing vault:

- evidence references carry source, session, observation time and confidence;
- repeated observations can be assessed for durable promotion without turning every frame or sentence into long-term memory;
- canonical observation privacy is admitted before candidate construction;
- relation beliefs have explicit observed/inferred/confirmed/disputed/superseded/expired lifecycle semantics;
- hybrid ranking can combine semantic similarity, relation affinity, recency, confidence, evidence strength and project affinity;
- graph expansion is breadth-first, depth/node bounded, and operates over one already-fetched graph snapshot to avoid N+1 queries;
- rejected, superseded and archived vault nodes are excluded from connected retrieval by default.

## Entity model

The existing vault intentionally has a fixed ten-type `NodeType` schema. Connected temporal reasoning needs additional projected entities without making old frontmatter unreadable, so `WorldEntityKind` is additive rather than a persistent-schema replacement.

It covers `User`, `Project`, `Activity`, `Goal`, `Task`, `Application`, `File`, `Person`, `Concept`, `Decision`, `Event`, `Outcome`, `Preference`, `Fact`, `Error`, `Session`, and `Note`.

Existing vault kinds map losslessly to and from the richer taxonomy. New projected-only kinds (`User`, `Activity`, `Application`, `File`, `Concept`, `Event`, `Outcome`) deliberately return no legacy `NodeType`; callers must not silently coerce them into generic notes merely to make them persistent. A future schema migration can make selected kinds durable with an explicit compatibility plan.

## Consolidation boundary

Producers such as temporal-session understanding should keep four stages distinct:

1. raw observations;
2. short-term session state;
3. candidate memory;
4. durable memory.

`world_model::assess_consolidation` is intentionally pure. It does not write to the vault. A producer proposes a `MemoryCandidate` containing redacted evidence references, and the policy returns one of:

- `reject_transient` — insufficient salience/confidence;
- `keep_candidate` — plausible but not independently established, or sensitive and lacking explicit confirmation;
- `promote_durable` — enough recent, deduplicated evidence exists across distinct sessions.

The default policy requires three recent evidence references across at least two sessions. These defaults are not a permanent product contract: integration should surface the policy through Continuum configuration before runtime promotion is wired up.

A realistic synthetic case is repeated editing, testing and documentation work on Continuum across multiple sessions. That can qualify for durable project/activity context. One unrelated observation cannot.

## Evidence and privacy

Evidence references are identifiers, not raw activity payloads. Callers must preserve source provenance and sensitivity and must not serialize passwords, private screen contents, clipboard text or personal filenames into the relation contract.

Ambient/session producers must use `observation_adapter` before constructing memory input:

- `never_observe` is rejected before a memory candidate or evidence record exists;
- `local_only` may remain in bounded local history, but it is not eligible for automatic memory-candidate construction at all; a separate explicit-confirmation/manual path is required before it can become memory;
- `cloud_allowed` may enter ordinary candidate consolidation after the usual evidence checks.

`assess_observation_consolidation` enforces that boundary even if a generic manual-flow consolidation configuration allows sensitive memory promotion. Generic sensitive promotion is reserved for separately evidenced explicit-confirmation/manual flows, not ambient observation.

## Relation lifecycle

`ConnectedRelation` provides the richer contract needed by session reasoning and a future Memory UI without breaking legacy vault frontmatter. It includes:

- `from`, `to`, relation type and confidence;
- lifecycle state;
- zero or more evidence references;
- `valid_from` / `valid_until`;
- supersession and contradiction references.

`is_current_at` refuses disputed/superseded/expired relations, contradicted relations and relations outside their temporal validity window.

This structure is additive. Existing `Relation { to, rel, confidence }` frontmatter remains readable and writable, so no persistent-state migration is required for this first foundation. A later migration can enrich stored relations gradually while old notes remain valid.

## Retrieval

`rank_hybrid` reranks an already-bounded candidate set. Vector/FTS retrieval remains responsible for candidate generation; the world-model layer adds relationship and evidence signals rather than replacing semantic retrieval.

The score is explainable: each returned hit includes individual semantic, relation, recency, confidence, evidence and project components. Superseded/rejected/archived nodes are filtered before ranking.

`bounded_related_nodes` expands relationships in memory from a single `GraphData` snapshot. It is both depth- and node-capped and applies a minimum edge confidence. This is the intended performance contract for A7 integration: fetch a bounded graph once, then traverse locally rather than issuing one relation query per node.

LanceDB remains useful for episodic vector candidate generation. It should not become an authority on whether a fact or relation is current; lifecycle truth stays in the vault/world-model projection.

## UI contract

A6 can render real connected data using the existing `GraphData` plus the additive `WorldEntity` and `ConnectedRelation` contracts. UI code should never fabricate evidence, confidence or lifecycle state. Until enriched relations are persisted, the UI should clearly distinguish existing vault edges (type/confidence/origin) from richer evidence-backed relation records and projected-only entities.

## Compatibility and migration

This change is intentionally migration-free:

- no authoritative markdown schema field is made mandatory;
- no SQLite schema change is required;
- existing MCP memory tools remain unchanged;
- the disposable graph index continues to rebuild from legacy and current vault notes;
- the legacy semantic SQLite store remains a read-only migration/fallback input rather than a second source of truth.

When enriched relation/entity persistence is added later, it must preserve unknown frontmatter keys, rebuild safely, include schema-version recovery, and retain old notes without silent loss.

## Current limitation

The policy and ranking primitives are implemented and tested as deterministic library code, but the temporal-session producer does not yet call the consolidation policy and the vault does not yet persist `ConnectedRelation` or projected-only entity kinds as first-class enriched state. This separation is deliberate for the swarm: A3 can produce candidate evidence without writing durable memory directly, and A1 can wire the producer/storage boundary after selecting compatible changes.
