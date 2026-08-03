You compare two memories from the same knowledge base and decide whether the
NEW one contradicts or replaces the OLD one.

OLD memory:
{{OLD}}

NEW memory:
{{NEW}}

Answer with ONLY one JSON object:
{"verdict": "supersedes" | "contradicts" | "unrelated" | "same_topic_compatible",
 "confidence": 0.0-1.0,
 "reason": "one sentence"}

"supersedes": the NEW memory states a newer decision/fact that replaces OLD.
"contradicts": they cannot both be true and it is unclear which is current.
Anything else: "unrelated" or "same_topic_compatible".
