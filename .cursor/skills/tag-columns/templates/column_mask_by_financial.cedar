// Starter template: financial column mask, tag-scoped.
//
// NOT YET SHIPPED in examples/policies/. Adopt this template when
// the catalog adds a finance-sensitive table — typical columns:
// `account_number`, `balance`, `card_number`. Regenerate the
// manifest with ./scripts/compile-policies.sh and publish to UC.
//
// The `finance` role carve-out is an opt-in: remove it if the
// deployment wants finance-role analysts to see all financial
// columns without masking.

@id("column_mask_by_financial_tag")
@filter_type("column_mask")
@applies_to_tag("financial")
@description("Mask columns tagged `financial` for non-admin, non-finance users")
forbid (
    principal,
    action == Action::"query",
    resource
)
unless {
    principal.role == "admin" || principal.role == "finance"
};
