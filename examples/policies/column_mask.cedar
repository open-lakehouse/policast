// Column-mask templates.
//
// These two templates replace what used to be a pair of per-column
// forbid rules (`column_mask_ssn`, `column_mask_diagnosis`). Instead of
// naming individual columns, each template targets a *tag* in the
// governance tag index — and the resolver expands the template into one
// concrete column_mask policy per matching `(table, column)` tuple.
//
// To add a new sensitive column, a data owner now only has to tag the
// column in `governance.policast.tags`; no Cedar edit, no redeploy.
//
// Tag taxonomy used here:
//   * "pii" — personally identifiable information (SSN, SIN, DOB, ...)
//   * "phi" — protected health information (diagnosis, treatment, ...)
//
// Both tags happen to carve out the same set of roles (admin,
// physician) today, but splitting them keeps the option open to
// evolve their access lists independently (e.g. physicians can see
// phi but not pii) without touching the non-tag-scoped policy set.

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
