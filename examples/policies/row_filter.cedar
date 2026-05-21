// Row-level filters.
//
// `row_filter_region` is authored as a **template**: the
// `@target_tag("clinical")` annotation means the resolver expands this
// rule into one concrete row filter per table carrying the `clinical`
// tag in the governance tag index. Tag a new clinical table and this
// analyst-regional-isolation rule starts applying to it automatically,
// with no Cedar edit.
//
// `row_filter_physician` stays as a concrete rule targeted at
// `patients`: its CEL predicate references `resource.treating_physician`,
// a column specific to patient-shaped tables. Templating that blindly
// across the whole `clinical` tag would evaluate the predicate against
// tables that don't have that column, so authoring it concretely is
// the correct call here. Mix-and-match is the point: tag-scope what
// generalizes, hand-author what doesn't.

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
