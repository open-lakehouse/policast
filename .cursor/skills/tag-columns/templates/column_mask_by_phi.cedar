// Starter template: PHI column mask, tag-scoped.
//
// Drop into examples/policies/ and regenerate the manifest with
// ./scripts/compile-policies.sh before publishing to UC. Mirrors
// the shipped `column_mask_by_phi_tag` policy.

@id("column_mask_by_phi_tag")
@filter_type("column_mask")
@applies_to_tag("phi")
@description("Mask columns tagged `phi` for non-admin, non-physician users")
forbid (
    principal,
    action == Action::"query",
    resource
)
unless {
    principal.role == "admin" || principal.role == "physician"
};
