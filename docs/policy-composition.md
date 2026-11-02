# Policy composition

Policast distinguishes alternative access grants from mandatory restrictions:

| Kind | Cedar effect | Composition across applicable policies |
| --- | --- | --- |
| `row_grant` | `permit` | OR: any grant can admit a row |
| `row_filter` | `permit` | AND: every restriction must pass |
| `deny_override` | `forbid` | Any TRUE deny excludes the row |
| `column_mask` | `forbid` | Independent of grants; any applicable mask protects the column |

For a table with configured grants, visibility is
`OR(grants) AND AND(row_filters) AND NOT OR(denies)`.
No eligible grant means no rows. Tables without grants retain legacy filter-only
behavior; an empty legacy manifest is not a default-deny authorization system.

Use `policast new --kind row-grant --table patients` to scaffold an alternative
grant. See [the composition example](../examples/composition.cedar).
Existing `row_filter` policies remain restrictions, not grants.

Policy role/principal constraints are AND-combined. Alternative UC bindings are
OR-combined, then AND-combined with those policy constraints. Binding precedence
orders audit output; it cannot override a deny or remove a mandatory restriction.
The resolver expands tag policies before enforcement. Unexpanded templates are
rejected by engines.

Only TRUE admits a row through a grant/restriction or triggers a deny.
NULL therefore fails grants/restrictions and does not itself trigger a deny.
Applicable malformed CEL, missing attributes, unsupported constructs, invalid
effects, and mask evaluation failures reject the query, even when another grant
is TRUE. Ineligible policies are excluded before expression evaluation.

The portable expression subset is literals, direct `resource.column` and
`principal.attribute` references, comparisons, boolean operators, and arithmetic.
Masks use principal attributes and the known `resource.table_name`; row-dependent
masking and arbitrary CEL functions/macros are not supported.

Composition manifests use schema version 2. `row_grant_targets` preserves table
grant requirements across principal selection; `binding_selectors` preserves UC
binding eligibility during materialization. JSON and protobuf retain this
metadata, including when no eligible policies remain. Group-bound Redis entries
require authoritative resolution when group claims are unavailable.

Upgrade Rust and Spark readers before publishing version-2 manifests. Older
readers cannot safely enforce new semantics. Do not downgrade readers while
composition policies remain deployed.

Both adapters execute the shared conformance fixtures under
`tests/fixtures/composition/cases.json`, including policy reversal and duplication.
