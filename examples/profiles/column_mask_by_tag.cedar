// ============================================================================
// PROFILE: column_mask
// ============================================================================
//
// A column_mask replaces a sensitive column's values with "***" for principals
// who are not exempt. It is the Cedar `forbid` + `unless` idiom — RBAC: deny by
// default, carve out the roles that are allowed to see the cleartext.
//
// Shape enforced by the policast compiler:
//   * effect MUST be `forbid`
//   * MUST target a column: either a concrete `@column("ssn")` OR a governance
//     tag with `@applies_to_tag("pii")` (mutually exclusive). Tag-scoped masks
//     are expanded by the resolver into one concrete (table, column) mask per
//     tagged column — tag a new column and the mask applies automatically.
//   * SHOULD have an `unless` carve-out (without one, every principal is
//     masked and the compiler will warn)
//
// Optionally narrow the tables in scope with `@target_table` / `@target_tag`.
//
// ---------------------------------------------------------------------------
// Example — mask any column tagged `pii` for everyone except admins and
// physicians.
// ---------------------------------------------------------------------------
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
