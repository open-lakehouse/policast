## Summary

<!-- What does this PR do and why? Focus on the "why". -->

Closes #

## Type of change

- [ ] `feat` — new functionality
- [ ] `fix` — bug fix
- [ ] `chore` — build / CI / deps / repo upkeep
- [ ] `refactor` — no behavior change
- [ ] `test` — tests only
- [ ] `docs` — documentation only

## Testing checklist

<!-- See CONTRIBUTING.md — every change ships with tests. -->

- [ ] Added or updated tests covering this change (happy path **and** edge/error cases)
- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --workspace --all-features` passes
- [ ] Spark changes: `sbt test` passes in `policast-spark/`
- [ ] Cedar policy changes: regenerated `examples/policies/manifest.json` (`./scripts/compile-policies.sh` or `just compile`) and committed the result
- [ ] No passing tests were deleted

## Checklist

- [ ] Branch follows the naming convention (`feat/`, `fix/`, `chore/`, `refactor/`, `test/`)
- [ ] Commits are signed off (DCO: `git commit -s`)
- [ ] Documentation updated where relevant (`docs/`, `README`, `AGENTS.md`)

## Notes for reviewers

<!-- Anything that needs special attention, follow-ups, or known limitations. -->
