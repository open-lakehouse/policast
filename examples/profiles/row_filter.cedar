// ============================================================================
// PROFILE: row_filter
// ============================================================================
//
// A row_filter keeps only the rows a principal is allowed to see. It is the
// Cedar `permit` + `when` idiom — textbook ABAC (compare a resource attribute
// to a principal attribute) or ReBAC (match a principal to an ownership
// column on the row).
//
// Shape enforced by the policast compiler:
//   * effect MUST be `permit`
//   * SHOULD have at least one `when` clause (an empty filter restricts
//     nothing and the compiler will warn)
//   * scope to a table with `@target_table("db.schema.table")` OR to a
//     governance tag with `@target_tag("clinical")` (mutually exclusive)
//
// Canonical principal vocabulary: principal.role, principal.region,
// principal.name, principal.groups. Other attributes compile but warn.
//
// ---------------------------------------------------------------------------
// Example A — ABAC: analysts only see rows in their own region.
// ---------------------------------------------------------------------------
@id("row_filter_region")
@filter_type("row_filter")
@target_tag("clinical")
@description("Restrict analysts to clinical rows in their assigned region")
@roles("analyst")
permit (
    principal,
    action == Action::"query",
    resource
)
when {
    resource.region == principal.region
};

// ---------------------------------------------------------------------------
// Example B — ReBAC: physicians only see their own patients.
// (Authored concretely against `patients` because the predicate references a
// patient-shaped column that not every clinical table has.)
// ---------------------------------------------------------------------------
@id("row_filter_physician")
@filter_type("row_filter")
@target_table("patients")
@description("Restrict physicians to their own patients")
@roles("physician")
permit (
    principal,
    action == Action::"query",
    resource
)
when {
    resource.treating_physician == principal.name
};
