---
name: Bug report
about: Report something that isn't working as expected
title: "bug: "
labels: bug
assignees: ''
---

## Summary

A clear and concise description of the bug.

## Affected component

- [ ] `policast-core` (Cedar parser / CEL emitter / CLI / manifest)
- [ ] `policast-datafusion` (DataFusion enforcement)
- [ ] `policast-uc` (Unity Catalog / sidecar)
- [ ] `policast-spark` (Spark plugin)
- [ ] Docs / examples / build / CI
- [ ] Other / not sure

## Steps to reproduce

Steps to reproduce the behavior. A **minimal** Cedar policy + query that
triggers the issue is the most helpful thing you can provide.

1.
2.
3.

```cedar
// minimal policy, if relevant
```

## Expected behavior

What you expected to happen (e.g. which rows/columns should have been
filtered or masked).

## Actual behavior

What actually happened. Include error output / logs in a code block.

## Environment

- policast version / commit:
- OS:
- Rust version (`rustc --version`), if relevant:
- JDK / Spark version, if relevant:

## Additional context

Anything else that might help — manifest excerpt, screenshots, related issues.
