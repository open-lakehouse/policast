// ============================================================================
// PROFILE: deny_override (legal_hold)
// ============================================================================
//
// A deny_override is a hard, service-wide guardrail that overrides any permit.
// It is the canonical Cedar forbid-overrides-permit pattern from the docs:
//
//     forbid ( principal, action, resource )
//     when   { resource.tags.contains("private") }   // what to deny
//     unless { resource in principal.account };       // who is exempt
//
// policast inverts this at the engine: rows matching the `when` guard are
// dropped for everyone except principals matched by the `unless` exemption
// (for whom the filter is constant-folded away).
//
// Shape enforced by the policast compiler:
//   * effect MUST be `forbid`
//   * SHOULD have a `when` guard naming the resource condition to deny
//     (without one it denies unconditionally and the compiler will warn)
//   * `unless` names the principals exempt from the deny
//
// ---------------------------------------------------------------------------
// Example — records under legal hold are invisible to everyone but the legal
// team.
// ---------------------------------------------------------------------------
@id("deny_legal_hold")
@filter_type("deny_override")
@target_table("patients")
@description("Block access to records under legal hold unless user has legal role")
forbid (
    principal,
    action == Action::"query",
    resource
)
when {
    resource.legal_hold == true
}
unless {
    principal.role == "legal"
};
