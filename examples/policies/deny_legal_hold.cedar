// Deny override: No one may access records under legal hold unless they
// have the "legal" role. This is a hard deny that overrides any permit.
@id("deny_legal_hold")
@filter_type("deny_override")
@target_table("patients")
@description("Block access to records under legal hold unless user has legal role")
forbid (
    principal,
    action,
    resource
)
when {
    resource.legal_hold == true
}
unless {
    principal.role == "legal"
};
