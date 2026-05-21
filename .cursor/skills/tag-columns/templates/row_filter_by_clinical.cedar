// Starter template: table-level row filter, tag-scoped on `clinical`.
//
// Mirrors examples/policies/row_filter.cedar::row_filter_region but
// with a distinct @id so the two can coexist. Use this as the
// scaffold for *new* regional/tenant/role-scoping predicates that
// should apply across every clinical-tagged table.
//
// The predicate below is the trivial identity filter — replace
// `true` with your actual CEL-expressible guard (e.g. a tenant
// check, a consent predicate). Keep the @target_tag("clinical")
// annotation: that's what makes this a table-level template the
// resolver fans out across the tag index.

@id("row_filter_clinical_default")
@filter_type("row_filter")
@target_tag("clinical")
@description("Default row filter applied to every clinical-tagged table — replace the predicate with your policy")
@roles("analyst")
permit (
    principal,
    action == Action::"query",
    resource
)
when {
    true
};
