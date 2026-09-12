---
name: agz-rust-refactor
description: Refactor or remove unnecessary Rust code with AGZ Rust MCP using semantic references and write-free edit proposals.
---

# Refactor and cleanup

Use the connected AGZ Rust MCP with an absolute `dir` for the active checkout.
Identify the behavior to preserve and read the owning code and tests. Use
`references`, `implementations` or a bounded `hierarchy` for the symbol involved.
Check macro/cfg-generated and public consumers before inferring unused code;
an empty or unavailable analyzer result is not proof that removal is safe.

Use `rename` for symbol renames and `refactor` for available semantic actions.
Both produce write-free patches. Check skipped/unsupported edits, original
text, affected files and completeness before applying. For a signature change,
`change(action="migrate")` can prepare a bounded workspace-consumer edit package;
read its live schema and unresolved consumers. Recompute proposals after source
changes instead of applying stale edit coordinates.

Keep cleanup focused on the requested flow. Prefer removing repeated work or
dead branches backed by source evidence over adding generalized abstractions.
Use `audit` selectively for advisory findings; do not convert all findings into
an unrelated cleanup campaign.

Stage complex candidates with `change`, or use `work` with the
`refactor_and_verify` template and explicit scope/gates. The host still reviews
and applies the exported patch. Validate changed consumers and behavior using
`check` or `verify`; include feature-gated consumers where relevant and retain
the repository's delivery requirements. Report intentional API changes and
consumers that could not be verified.
