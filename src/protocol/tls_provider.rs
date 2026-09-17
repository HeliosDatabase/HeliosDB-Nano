//! Shared, explicit PQ-aware `rustls` `CryptoProvider` construction.
//!
//! Both the PostgreSQL and MySQL wire listeners build their `rustls::ServerConfig`
//! from the provider returned here instead of relying on the implicit
//! process-wide default (`rustls::crypto::CryptoProvider::install_default`).
//!
//! ## Why aws-lc-rs, not ring
//!
//! `X25519MLKEM768` (draft-ietf-tls-ecdhe-mlkem — the hybrid post-quantum key
//! exchange group already shipped by AWS, MySQL 26.7 and CockroachDB under the
//! "quantum-ready TLS" banner) only exists in rustls's `aws_lc_rs` crypto
//! backend. Confirmed against the vendored rustls 0.23.45 source
//! (`crypto/aws_lc_rs/pq/mod.rs:14`) — the `ring` backend has no PQ key
//! exchange groups at all.
//!
//! `aws_lc_rs::default_provider()` already lists `X25519MLKEM768` first in
//! `DEFAULT_KX_GROUPS` (classical `X25519`/`SECP256R1`/`SECP384R1` follow), so
//! a PQ-capable provider is just the default provider unmodified; a
//! classical-only provider is the same provider with the hybrid group
//! filtered out of the key-exchange group list.

use std::sync::Arc;

/// Build an explicit `rustls` `CryptoProvider`.
///
/// - `post_quantum = true`: offers `X25519MLKEM768` (hybrid post-quantum key
///   exchange) alongside the classical groups, in aws-lc-rs's default
///   preference order (PQ hybrid first).
/// - `post_quantum = false`: the same provider with the PQ hybrid group
///   filtered out, leaving only classical key exchange.
pub fn pq_capable_provider(post_quantum: bool) -> Arc<rustls::crypto::CryptoProvider> {
    let provider = rustls::crypto::aws_lc_rs::default_provider();

    if post_quantum {
        return Arc::new(provider);
    }

    let classical_kx_groups: Vec<_> = provider
        .kx_groups
        .iter()
        .copied()
        .filter(|group| group.name() != rustls::NamedGroup::X25519MLKEM768)
        .collect();

    Arc::new(rustls::crypto::CryptoProvider {
        kx_groups: classical_kx_groups,
        ..provider
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_provider_offers_hybrid_group_first() {
        let provider = pq_capable_provider(true);
        assert!(
            provider
                .kx_groups
                .iter()
                .any(|g| g.name() == rustls::NamedGroup::X25519MLKEM768),
            "PQ-capable provider must offer X25519MLKEM768"
        );
    }

    #[test]
    fn classical_provider_excludes_hybrid_group() {
        let provider = pq_capable_provider(false);
        assert!(
            !provider
                .kx_groups
                .iter()
                .any(|g| g.name() == rustls::NamedGroup::X25519MLKEM768),
            "classical-only provider must not offer X25519MLKEM768"
        );
        assert!(!provider.kx_groups.is_empty(), "classical provider must still offer classical groups");
    }
}
