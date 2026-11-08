# Security maintenance exceptions for 4.42.0

Status: maintenance-policy adoption authorized for the4.42.0 release-source successor on2026-11-08. The paired TOML files take effect when committed; security and release acceptance remain pending actual final-source gates. Owner: Codex / HeliosDB release follow-up. New paste remediation: Sprinter12d7fb448cd0 (open). Review by2026-12-08 and before each release.

The release keeps the exact `cargo audit --deny warnings` command and both fatal default/all-feature cargo-deny commands. cargo-audit and cargo-deny now share the following maintenance-only IDs. The first four are existing deny.toml policy; adding them to audit.toml aligns that policy. paste is an explicitly NEW fifth exception whose remaining maintenance risk is accepted for this release.

| Advisory ID | Package | Reason and removal work |
|---|---|---|
| RUSTSEC-2025-0141 | bincode | Existing on-disk/value codec. Replace through a storage-format migration with compatibility proof, not a dependency bump to suppress a warning. |
| RUSTSEC-2025-0134 | rustls-pemfile | Existing migration to rustls-pki-types remains tracked. |
| RUSTSEC-2025-0119 | number_prefix | Existing indicatif leaf dependency used for display. Remove through a compatible dependency update. |
| RUSTSEC-2024-0449 | unsafe-libyaml | Existing serde_yaml backend. Parse inputs are the compiled-in OpenAPI spec; EXPLAIN YAML output is engine-built. Replace with output-compatibility validation. |
| RUSTSEC-2024-0436 | paste | NEW: optional code-embed -> FastEmbed7.5.0 -> Tokenizers0.23.2 -> paste1.0.15 in reviewed source32. No reachable default-feature paste edge. Remove through a compatible published Tokenizers maintenance release or fully publishable compatible fork chain. |

The current RustSec records classify all five as `informational = "unmaintained"`, with no patched-version list. This records a maintenance-risk decision, not a claim that the packages or resulting build are vulnerability-free. paste is trusted build-time procedural-macro/build-script code; expanded Tokenizers code still runs during inference. Its archived upstream may not fix future defects. Upgrading macro_rules_attribute already removed its independent paste edge; only direct Tokenizers remains. No compatible published stable Rust Tokenizers/FastEmbed replacement was available in the captured source32 review.

Ignore entries match advisory IDs, not dependency edges, versions or advisory categories. Neither TOML file mechanically enforces the graph scope, review date or current maintenance classification. An ignored warning is filtered from the audit report; it is not promised to remain visible in normal output. Preserve the exception inventory, exact dependency graph/checksums and current advisory records as release evidence. Re-review if an ID's classification changes, a package version/parent changes, or paste becomes default-reachable. Do not carry any of these IDs forward as an exemption for a newly reported vulnerability or unsoundness.

The existing RUSTSEC-2026-0235 rkyv audit entry is unchanged and separate: it is permitted only while the all-feature inverse graph remains empty. The existing webpki-roots>=1.0.10 proof remains required; no old webpki ignore is restored. No pin-utils ignore, crate-wide exemption, severity threshold, category suppression, yanked exemption, feature removal, license/source/crypto relaxation or root-only dependency patch is added.

Unignored vulnerabilities remain fatal. Unignored unsound/yanked/unmaintained warnings remain fatal under `--deny warnings`. cargo-audit0.22.2 also continues to fail on its own cargo-audit/rustsec self-advisories under that exact flag; the dependency ignore list does not disable that separate check. Both cargo-deny graphs retain their existing advisory enforcement. A new advisory ID remains fatal even when it concerns an exception-listed package.

Removal requires a compatible publishable dependency graph and downstream consumer resolution proof, then deletion of the matching IDs from BOTH tools. Revalidate tokenizer/model behavior, Rust1.92 code-embed compilation and actual384D inference, regression, package and current security gates. Do not close Sprinter12d7fb448cd0 from this proposal or from a green wrapper alone.
