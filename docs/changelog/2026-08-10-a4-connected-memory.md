# A4 connected-memory foundation

- Added evidence-backed candidate and relation contracts for connected memory without introducing a dedicated graph database.
- Added a richer additive world-entity taxonomy covering user, project, activity, goal, task, application, file, person, concept, decision, event and outcome while preserving the legacy persistent `NodeType` schema.
- Added recurrence/salience/confidence/sensitivity consolidation policy that keeps raw observations, session state, candidates and durable memory distinct.
- Added an explicit canonical observation-privacy admission boundary: `never_observe` is rejected before candidate construction, `local_only` cannot auto-promote through the ambient path, and only eligible `cloud_allowed` evidence enters normal consolidation.
- Added lifecycle and temporal-validity checks for connected relations.
- Added explainable hybrid ranking across semantic, relationship, recency, confidence, evidence and project signals.
- Added bounded graph traversal over one fetched snapshot to avoid N+1 relation queries.
- Added synthetic tests for durable promotion, transient rejection, sensitive-content handling, privacy admission, contradiction/expiry, entity compatibility, hybrid ranking and bounded traversal.
- Existing vault markdown, SQLite schema and MCP memory APIs remain backward compatible; enriched entity/relation persistence remains follow-up integration work.
