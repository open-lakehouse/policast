---
name: tag-columns
description: Classifies Unity Catalog tables and columns with governance tags (pii, phi, financial, clinical, public) and emits the right INSERT / MERGE / tags.json edit plus a matching Cedar template when no policy yet covers the tag. Use when the user asks to tag a column, tag a table, classify PII/PHI columns, add an entry to governance.policast.tags, or asks "what tag should this column have?" — i.e. the policast-cel tag-driven FGAC authoring flow from the data-owner's perspective.
---

# tag-columns

Drive tag-scoped Cedar template expansion by walking a data owner
through column-by-column and table-level classification against the
project's canonical tag vocabulary, then emitting the exact edit the
governance store needs.

## When to use

Activate on any of:
- "Help me tag columns in `<fully.qualified.table>`"
- "Classify this column as PII / PHI / financial / public"
- "What policies apply to `<table>` / `<column>`?"
- "Add a tag to `governance.policast.tags`"
- "Tag the `patients` table"
- "I added a new column, which tag does it need?"

## Storage mode — pick this first

The skill supports two storage targets. Decide the mode before the
classification loop starts:

| Mode | Trigger | Output |
|------|---------|--------|
| **Flat-file** (default) | `UC_ENDPOINT` env var is unset AND `examples/uc/store/tags.json` exists | Unified diff for `examples/uc/store/tags.json` (applied with `StrReplace`) |
| **UC / Delta** | `UC_ENDPOINT` env var is set OR the user mentions `governance.policast.tags` by name | A single `MERGE INTO governance.policast.tags` SQL block using `WHEN MATCHED` to restore (clear `retired_at`) and `WHEN NOT MATCHED` to insert |

If ambiguous, ask once:

```
Flat-file mode (edits examples/uc/store/tags.json) or UC mode
(emits a MERGE against governance.policast.tags)?
```

## Workflow

Copy this checklist and walk the user through it:

```
Tag-columns progress:
- [ ] Step 1: Locate the target schema (table + columns)
- [ ] Step 2: Classify the table (table-level tag, optional)
- [ ] Step 3: Classify each column (column-level tag, optional per column)
- [ ] Step 4: Produce the governance-store edit
- [ ] Step 5: Check template coverage, suggest a Cedar template if needed
```

### Step 1 — locate the schema

Flat-file mode: read the schema hint from
`examples/uc/properties/<table>.properties.json` if it exists, otherwise
ask the user to paste a `DESCRIBE <table>` output. In that file,
`schema` or `columns` carries the `(name, type)` pairs the skill needs.

UC mode: the schema lives in UC itself. Either:

```bash
curl -s "$UC_ENDPOINT/api/2.1/unity-catalog/tables/<full_name>" \
  | jq '.columns | map({name, type: .type_name})'
```

or `databricks unitycatalog tables get <full_name>` if the Databricks
CLI is available. If neither is wired, ask the user to paste the
`DESCRIBE` output just like flat-file mode.

Normalize the schema into a simple `[(column, type), ...]` list the
classification loop will walk.

### Step 2 — classify the table

Tags that apply at the table grain (entity_kind = `table`) are the
ones the `@target_tag(...)` templates key off of. The canonical set
today:

| Tag | Meaning |
|-----|---------|
| `clinical` | Table participates in patient-care workflows. Drives `row_filter_region`, row filters that require provider assignment, etc. |
| `financial` | Table holds money-movement records (transactions, invoices). |
| `public` | Table has no per-principal restrictions — governance is still tracked, just as a no-op marker. |

Ask exactly once: **"At the table grain, which of these tags apply
to `<full_name>`? (zero or more from: clinical, financial, public,
or a new custom tag name)."**

If the user proposes a brand-new tag name, capture it but confirm
the choice in Step 5 by pointing at `tag-vocabulary.md` — brand-new
tags only have operational meaning once at least one Cedar template
references them.

### Step 3 — classify each column

Tags that apply at the column grain (entity_kind = `column`) are
what `@applies_to_tag(...)` templates key off of. The canonical set
today:

| Tag | Meaning | Typical columns |
|-----|---------|-----------------|
| `pii` | Directly identifies an individual outside a clinical context. | `ssn`, `email`, `phone`, `home_address` |
| `phi` | Protected health information — medical record content. | `diagnosis`, `medication`, `treatment_notes` |
| `financial` | Account-level sensitive financial data. | `account_number`, `card_number`, `balance` |
| `public` | Safe for unrestricted reads; tagged for completeness. | `patient_id`, `region`, `created_at` |

For each `(column, type)` pair, ask in a **single compact question**:

```
<column_name> (<type>) → pii | phi | financial | public | skip
```

`skip` leaves the column untagged. This keeps the loop moving; a
40-column table takes ~40 seconds of confirmations.

### Step 4 — produce the governance-store edit

**Flat-file mode** — propose a single `StrReplace` against
`examples/uc/store/tags.json` that adds the new rows into the `rows`
array. Preserve alphabetical order by `entity` so the diff stays
reviewable. Use this row shape (matches `TagRow` in
`crates/policast-uc/src/backend.rs`):

```json
{
  "entity": "catalog.schema.table",
  "entity_kind": "table",
  "tag": "clinical",
  "set_by": "tag-columns-skill",
  "set_at": "<ISO-8601 timestamp at write time>",
  "retired_at": null
}
```

For columns, `entity` is `catalog.schema.table:column` and
`entity_kind` is `"column"`.

**UC mode** — emit one `MERGE` that covers every new row. This
handles both first-time inserts and revival of retired tags in a
single statement:

```sql
MERGE INTO governance.policast.tags AS t
USING (
    VALUES
        ('catalog.schema.table',        'table',  'clinical',   'data-owner@hospital.com', CURRENT_TIMESTAMP()),
        ('catalog.schema.table:ssn',    'column', 'pii',        'data-owner@hospital.com', CURRENT_TIMESTAMP())
) AS src(entity, entity_kind, tag, set_by, set_at)
ON t.entity = src.entity AND t.entity_kind = src.entity_kind AND t.tag = src.tag
WHEN MATCHED AND t.retired_at IS NOT NULL THEN
    UPDATE SET retired_at = NULL, set_by = src.set_by, set_at = src.set_at
WHEN NOT MATCHED THEN
    INSERT (entity, entity_kind, tag, set_by, set_at, retired_at)
    VALUES (src.entity, src.entity_kind, src.tag, src.set_by, src.set_at, NULL);
```

Ask the user for their identity once and thread it through as
`set_by` — do not default to a literal string like
`"governance_admin"`.

### Step 5 — template coverage check

For every `(tag, entity_kind)` pair the user just added, check
whether a matching Cedar template already exists in
`examples/policies/*.cedar` or under the governance catalog's
manifest. Pull the list by grepping for:

- `@applies_to_tag("<tag>")` — column-level coverage
- `@target_tag("<tag>")` — table-level coverage

**If covered** — print a single line telling the user which policy
will fire once the tag lands: *"`column_mask_by_pii_tag` will now
expand to `<new_expansion_id>` on the next resolve."*

**If not covered** — copy the matching starter template from
[`templates/`](templates) (see below), adjusting the tag name as
needed, and offer it as a `Write` into `examples/policies/` that
the user can then refine. Include a clear WARN line noting the
user must regenerate the manifest via
`./scripts/compile-policies.sh` before UC publish.

## Starter Cedar templates (progressive disclosure)

Canonical tag-scoped templates the skill can pull from when Step 5
discovers a coverage gap. Browse them in
[`templates/`](templates):

- `templates/column_mask_by_pii.cedar` — generic PII column mask,
  mirrors `examples/policies/column_mask.cedar::column_mask_by_pii_tag`.
- `templates/column_mask_by_phi.cedar` — PHI column mask.
- `templates/column_mask_by_financial.cedar` — financial column mask
  with an opt-in `finance` role exemption.
- `templates/row_filter_by_clinical.cedar` — table-level filter
  template that mirrors `row_filter_region` but is authored from
  scratch so it can be adapted to new attribute predicates.

For the vocabulary itself (who owns each tag, what the retirement
semantics are, how to add a new tag), see
[`tag-vocabulary.md`](tag-vocabulary.md).

## What NOT to do

- **Don't** default `set_by` to a placeholder string. Ask the user.
- **Don't** silently invent a brand-new tag (e.g. "confidential")
  without verifying no existing tag covers the intent. Run Step 5's
  grep first.
- **Don't** edit `governance.policast.tags` via repeated single-row
  `INSERT` statements. One `MERGE` per classification session keeps
  the Delta CDF output compact and the UcBootstrapBackend's refresh
  loop cheaper.
- **Don't** propose edits to `examples/uc/ddl/05_seed.sql` as part
  of the classification flow — that file is the deterministic demo
  seed, not an authoring surface. Touch it only if the user
  explicitly wants the demo to carry the new tags.

## Verification

After emitting the edit, confirm the expected expansion by showing
the user the new `row_filter_region@<table>` /
`column_mask_by_pii_tag@<table>:<column>` ids the resolver will emit
on the next resolve. This proves the classification will actually
change enforcement, not just sit as metadata.
