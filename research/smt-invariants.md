# SMT invariants for the UC policy-store model

Status: draft invariants (Stage 5 seed)  
Source model: [unity-catalog-policy-store.md](./unity-catalog-policy-store.md)

This document formalizes the core security claims from the UC policy-store
design into solver-friendly invariants. It is the spec input for a future
`policast analyze` command using `cedar-policy-symcc`.

## 1. Model sketch

We model:

- Principal \(p\) with attributes:
  - `role(p)` in {`admin`, `physician`, `analyst`, `legal`, ...}
  - `region(p)` (optional string)
  - `name(p)` (optional string)
- Resource row \(r\) with attributes:
  - `table(r)` fully-qualified table name
  - `region(r)`
  - `treating_physician(r)`
  - `legal_hold(r)` boolean
- Column \(c\) with:
  - `owner_table(c)`
  - `column_name(c)`
  - `has_tag(c, t)` for tag \(t\)
- Policy evaluation relation:
  - `row_visible(p, r)` — row survives all row filters + deny overrides
  - `column_masked(p, c)` — projection emits masked value (`***`)

Assume Cedar semantics:

- default deny
- `forbid` overrides `permit`
- policy order independence

## 2. Invariant set

### INV-001: pii-requires-physician

If a column is tagged `pii` and principal is neither physician nor admin,
the final projection must be masked.

\[
\forall p, c:\ has\_tag(c,\text{"pii"}) \land role(p)\notin\{\text{"physician"},\text{"admin"}\}
\Rightarrow column\_masked(p,c)
\]

Rationale: protects direct identifiers for non-clinical, non-admin users.

### INV-002: legal-hold-dominates-region

For rows under legal hold, the deny policy dominates any allow path that
would otherwise expose the row (including region-based row filters).

\[
\forall p, r:\ legal\_hold(r)=true \land role(p)\neq\text{"legal"}
\Rightarrow \neg row\_visible(p,r)
\]

Rationale: deny override must remain absolute for legal-hold isolation.

### INV-003: tag-expansion-monotonic

Adding a governance tag to a column/table may keep or reduce visibility,
but can never increase it.

Given two tag assignments \(T\subseteq T'\), with policies unchanged:

\[
\forall p, r, c:\ row\_visible_{T'}(p,r) \Rightarrow row\_visible_T(p,r)
\]

\[
\forall p, c:\ column\_masked_T(p,c) \Rightarrow column\_masked_{T'}(p,c)\ \lor\ column\_masked_{T'}(p,c)=column\_masked_T(p,c)
\]

Operationally: tag expansion is a restriction-only transformation.

## 3. Counterexample shape (what to emit on failure)

For each violated invariant, emit:

1. principal assignment (`id`, `role`, attrs),
2. resource/column assignment,
3. matched policy ids (`bindings_applied` + `expanded_from` when present),
4. minimal witness showing violation.

Example witness for INV-001:

- principal role=`analyst`
- column=`hospital.clinical.patients:ssn`, tags={`pii`}
- evaluation result `column_masked=false`
- violated because non-physician/non-admin saw raw pii column

## 4. Mapping to current codebase

- Tag expansion source: `policast-uc/src/store.rs` (`expand_tag_scoped`)
- Resolved policy payload: `policast-uc/src/types.rs` (`ResolveBundle`)
- DataFusion mask application: `policast-datafusion/src/governance_table.rs`
- Cedar compilation: `policast-core/src/policy_manifest.rs`

These are the anchor points where future solver traces should reference
policy ids and evaluated expressions.

## 5. Implementation notes for `policast analyze` (future)

1. Input:
   - compiled manifest snapshot
   - bindings snapshot
   - tags snapshot (already expanded or expand-on-load)
2. Symbolic domains:
   - finite role set (from observed principals + configured roles)
   - bounded string domains for table names and tags
3. Evaluation:
   - translate each CEL predicate into symbolic constraints
   - assert invariant negation and call SAT/SMT solver
4. Output:
   - `holds` or `counterexample` with witness payload

This keeps the invariants declarative while leaving engine-specific
expression lowering to the analyzer implementation.
