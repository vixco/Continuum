# A4 connected-memory foundation

- Added evidence-backed candidate and relation contracts for connected memory without introducing a dedicated graph database.
- Added recurrence/salience/confidence/sensitivity consolidation policy that keeps raw observations, session state, candidates and durable memory distinct.
- Added lifecycle and temporal-validity checks for connected relations.
- Added explainable hybrid ranking across semantic, relationship, recency, confidence, evidence and project signals.
- Added bounded graph traversal over one fetched snapshot to avoid N+1 relation queries.
- Added synthetic tests for durable promotion, transient rejection, sensitive-content handling, contradiction/expiry, hybrid ranking and bounded traversal.
- Existing vault markdown, SQLite schema and MCP memory APIs remain backward compatible; enriched relation persistence remains follow-up integration work.
