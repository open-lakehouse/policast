// Starter template: PII column mask, tag-scoped.
//
// Drop into examples/policies/ and regenerate the manifest with
// ./scripts/compile-policies.sh before publishing to UC. Mirrors
// the shipped `column_mask_by_pii_tag` policy so the two don't
// collide — rename the @id if you want both to coexist, otherwise
// use this as a clean replacement.

@id("column_mask_by_pii_tag")
@filter_type("column_mask")
@applies_to_tag("pii")
@description("Mask columns tagged `pii` for non-admin, non-physician users")
forbid (
    principal,
    action == Action::"query",
    resource
)
unless {
    principal.role == "admin" || principal.role == "physician"
};
