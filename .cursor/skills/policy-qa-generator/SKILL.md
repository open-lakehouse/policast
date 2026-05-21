---
name: policy-qa-generator
description: Generates Cedar governance policies through a short Q&A interview, then emits the exact artifacts needed for policast-cel (Cedar source, manifest update command, and binding/tag changes). Use when the user asks for a policy wizard, policy questionnaire, “ask me questions and generate policy”, or needs help drafting row filters/column masks/deny overrides from requirements.
---

# policy-qa-generator

Turn policy intent into production-ready governance edits by asking a
small set of targeted questions first, then generating concrete outputs.

## When to use

Activate for prompts like:

- “Create a policy generator”
- “Ask me questions and build the policy”
- “I need a row filter / mask policy but don’t know Cedar syntax”
- “Generate Cedar policy for this table/access rule”

Use this skill for **authoring**. Use `tag-columns` when the user is
only classifying table/column tags.

## Workflow

Copy this checklist and track progress:

```
Policy-QA progress:
- [ ] Step 1: Choose storage mode + scope
- [ ] Step 2: Collect policy intent with 8 focused questions
- [ ] Step 3: Emit Cedar draft (and template vs concrete choice)
- [ ] Step 4: Emit governance-store change (bindings / tags)
- [ ] Step 5: Emit verification commands + expected behavior
```

## Step 1 — choose storage mode + scope

Ask once:

1. `Mode`: flat-file (`examples/uc/store`) or UC/Delta (`governance.policast.*`)?
2. `Scope`: one table only, or tag-scoped template for multiple tables/columns?

If unclear, default to:

- mode = flat-file
- scope = one table

## Step 2 — ask the 8 policy questions

Ask in this order, one compact question each:

1. **Target** — full table name (`catalog.schema.table`)?
2. **Policy type** — `row_filter` | `column_mask` | `deny_override`?
3. **Principal selector** — role/group/principal wildcard?
4. **Predicate** — plain-English condition on `principal.*` and `resource.*`?
5. **Effect** — permit/forbid behavior and exceptions?
6. **Template?** — concrete target or tag-scoped (`@target_tag` / `@applies_to_tag`)?
7. **Policy id + precedence** — stable ID and binding order?
8. **Audit metadata** — author identity for `set_by` / `created_by` fields?

If the user gives partial answers, proceed with explicit assumptions.

## Step 3 — emit Cedar draft

Always emit one Cedar block and state whether it is:

- **concrete** (`@target_table`, optional `@column`) or
- **template** (`@target_tag` or `@applies_to_tag`)

Rules:

- Use existing annotation style from `examples/policies/*.cedar`.
- Keep generated IDs deterministic and reviewable.
- Prefer tag-scoped templates when the user wants repeatability across
  many columns/tables.

## Step 4 — emit governance-store changes

Emit the **smallest complete change set** for chosen mode.

### Flat-file mode

Provide:

1. Cedar file patch (or append snippet target path).
2. `scripts/compile-policies.sh` command.
3. `examples/uc/store/bindings.json` edit.
4. `examples/uc/store/tags.json` edit if template scope requires tags.

### UC/Delta mode

Provide:

1. Cedar snippet (source-of-truth for review).
2. SQL `MERGE` for `governance.policast.bindings`.
3. SQL `MERGE` for `governance.policast.tags` when tag-scoped.
4. Note that manifest must be republished (compile + publish path).

Never emit many one-row INSERTs when one MERGE is enough.

## Step 5 — verification plan

Always end with:

1. One command/query to validate policy materialization.
2. One resolve call scenario per relevant role.
3. Expected result summary in plain language.

For sidecar flows, include a `/policies/resolve` check and expected
`bindings_applied` / `expanded_from` behavior for templates.

## Output contract

Return results in this order:

1. **Assumptions** (if any)
2. **Generated Cedar policy**
3. **Governance-store updates** (flat-file diff or SQL MERGE)
4. **Verification commands**
5. **Next action** (compile/publish/run demo)

## Guardrails

- Do not invent table/column names; ask when missing.
- Do not generate policies without principal selector context unless the
  user explicitly wants wildcard.
- Do not skip manifest compile/publish reminders.
- Do not mutate unrelated demo seed files unless the user asks.
