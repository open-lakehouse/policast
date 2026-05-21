# Tag vocabulary

Reference sheet for the `tag-columns` skill. The canonical tag set
is intentionally small; adding a new tag is a deliberate governance
act, not a data-owner convenience.

## Table-level tags (entity_kind = "table")

Table tags drive `@target_tag(...)` templates. Every template that
keys off one of these tags will fan out across every table carrying
it, so adding a tag to a new table is a one-line governance change.

| Tag | Owner | Meaning | Retirement semantics |
|-----|-------|---------|----------------------|
| `clinical` | Clinical data steward | Table participates in patient-care workflows. Today: drives `row_filter_region`. | Retirement removes only the regional-isolation filter; does not affect column masks. |
| `financial` | Finance data steward | Table holds money-movement records. Reserved for a future `row_filter_by_financial` template — *not yet wired*. | Retirement is a no-op until a template references it. |
| `public` | Catalog-owner | No per-principal row restrictions; audit-only governance marker. | Retirement has no enforcement effect. |

## Column-level tags (entity_kind = "column")

Column tags drive `@applies_to_tag(...)` templates.

| Tag | Owner | Meaning | Retirement semantics |
|-----|-------|---------|----------------------|
| `pii` | Privacy steward | Directly identifies an individual outside a clinical context (SSN, email, phone, home address). | Retirement drops that column from `column_mask_by_pii_tag` expansion. |
| `phi` | HIPAA / compliance steward | Protected health information — anything that would be HIPAA-covered. | Retirement drops that column from `column_mask_by_phi_tag`. |
| `financial` | Finance data steward | Account-level sensitive financial data (account number, balance, card number). | Reserved for a future `column_mask_by_financial_tag` — not yet wired. |
| `public` | Catalog-owner | Safe to expose; tagged for completeness. | Retirement has no enforcement effect. |

## Adding a new tag

A new tag is governance-meaningful only once **at least one Cedar
template references it**. The authoring flow is:

1. Propose the tag here in `tag-vocabulary.md` with an owner and a
   retirement semantic.
2. Add a Cedar template under `examples/policies/` that keys off
   `@target_tag("<new>")` or `@applies_to_tag("<new>")`.
3. Regenerate the manifest with `./scripts/compile-policies.sh`.
4. Publish the updated manifest via `policast uc publish` (or mount
   it into the compose bootstrap for demos).
5. Only now: `INSERT` or `MERGE` tag rows referring to the new tag.

Doing these out of order produces inert tag metadata — rows in
`governance.policast.tags` that no policy consults. The skill is
allowed to propose a new tag but must stop short of writing rows
against it until the template lands.

## Retirement, not deletion

Tag rows are never hard-deleted. Set `retired_at` to the retirement
timestamp and keep the row. This preserves:

- **Audit history** — `expanded_from` entries in past
  `ResolveBundle`s continue to make sense.
- **CDF-based invalidation** — the `UcBootstrapBackend` CDF tail
  sees the retire as an UPDATE, not a DELETE, and can invalidate
  cached expansions precisely.
- **Revival** — restoring the tag is an UPDATE that clears
  `retired_at`; the skill's `MERGE` in Step 4 handles this branch
  automatically.

## Provenance (`set_by`)

Always a real identity — human email, service-principal id, or a
clearly-scoped automation tag like `tag-columns-skill:alice@hospital.com`.
Never a generic literal like `"governance_admin"`. The resolver's
`expanded_from` audit map does not carry `set_by`, but governance
dashboards that join the tag rows back against the resolver output
rely on provenance being truthful.
