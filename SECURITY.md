# Security Policy

We take the security of policast seriously. Because policast compiles and
enforces **data-governance policies**, a vulnerability here can translate
directly into unauthorized data access — so we appreciate responsible
disclosure.

## Supported versions

policast is pre-1.0 and under active development. Security fixes are applied to
the `main` branch. Until a formal release cadence is established, only the
latest `main` is supported.

| Version | Supported |
|---------|-----------|
| `main` (latest) | :white_check_mark: |
| older commits | :x: |

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Instead, report privately using one of the following:

- GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
  ("Report a vulnerability" under the repository's **Security** tab), or
- Email the maintainers at
  <!-- TODO(maintainers): replace with a monitored security contact, e.g. security@example.org -->
  **`TODO: ADD SECURITY CONTACT`**.

Please include:

- A description of the vulnerability and its impact (e.g. row filter / column
  mask bypass, policy evaluation error, privilege escalation).
- Steps to reproduce, ideally with a minimal Cedar policy and query.
- The affected component (`policast-core`, `policast-datafusion`,
  `policast-uc`, or `policast-spark`) and commit / version.

## What to expect

- **Acknowledgement** of your report as soon as we can triage it.
- An assessment and, where applicable, a coordinated fix and disclosure
  timeline.
- Credit for the discovery if you wish (let us know how you'd like to be
  attributed).

> **Maintainer decision (TODO):** define concrete response SLAs (e.g.
> acknowledge within N business days) once a security contact is in place.

Thank you for helping keep policast and its users safe.
