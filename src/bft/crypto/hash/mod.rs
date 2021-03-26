//! Abstractions over different crypto hash digest algorithms.

#[cfg(feature = "crypto_hash_ring_sha2")]
mod ring_sha2;

/// The type `Context` represents an on-going hash digest calculation.
pub struct Context {
    #[feature = "crypto_hash_ring_sha2"]
    inner: ring_sha2::Context,
}

/// Represents a hash digest.
#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct Digest {
    #[feature = "crypto_hash_ring_sha2"]
    inner: ring_sha2::Digest,
}

impl AsRef<[u8]> for Digest {
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}
