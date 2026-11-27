//! The proofs of the payloads, by the block hash of the payload, which is unique for a `VALID`
//! payload. A proof is claimed before its generation, so every proof is generated once.

use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use alloy_primitives::B256;
use bytes::Bytes;
use lru::LruCache;

use crate::proof::proof_type::ProofType;

/// Slots of proofs that are kept, two epochs, for a payload that is imported again after a
/// reorg.
const SLOTS: usize = 64;

/// One lock guards both parts, so a proof is never claimed two times.
pub(crate) struct ProofCache(Mutex<Proofs>);

struct Proofs {
    /// A claimed proof is not in the LRU cache, so it is never evicted.
    claimed: HashSet<(B256, ProofType)>,
    proven: LruCache<(B256, ProofType), Bytes>,
}

pub(crate) enum ClaimOutcome {
    Proven(Bytes),
    /// Claimed by an earlier request of the payload.
    Claimed,
    New(Claim),
    /// The proof is free, but the request has no witness to prove it with.
    NoWitness,
}

/// When a claim drops, the proof is free for a later request unless it is proven. So a proof
/// task that fails or stops frees its claim.
pub(crate) struct Claim {
    cache: Arc<ProofCache>,
    key: (B256, ProofType),
}

impl ProofCache {
    pub(crate) fn new(proof_type_count: usize) -> Arc<Self> {
        let capacity = NonZeroUsize::new(SLOTS * proof_type_count.max(1)).unwrap();
        Arc::new(Self(Mutex::new(Proofs {
            claimed: HashSet::new(),
            proven: LruCache::new(capacity),
        })))
    }

    /// Claims a proof for a request that can prove it. A request that cannot prove, because
    /// the EL sent it no witness, never takes the claim from one that can.
    pub(crate) fn claim(
        self: &Arc<Self>,
        block_hash: B256,
        proof_type: ProofType,
        has_witness: bool,
    ) -> ClaimOutcome {
        let key = (block_hash, proof_type);
        let mut proofs = self.0.lock().unwrap();
        if let Some(proof) = proofs.proven.get(&key) {
            return ClaimOutcome::Proven(proof.clone());
        }
        if proofs.claimed.contains(&key) {
            return ClaimOutcome::Claimed;
        }
        if !has_witness {
            return ClaimOutcome::NoWitness;
        }
        proofs.claimed.insert(key);
        ClaimOutcome::New(Claim {
            cache: self.clone(),
            key,
        })
    }
}

impl Claim {
    pub(crate) fn complete(self, proof: Bytes) {
        self.cache.0.lock().unwrap().proven.put(self.key, proof);
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.cache.0.lock().unwrap().claimed.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use bytes::Bytes;

    use crate::proof::{
        proof_cache::{ClaimOutcome, ProofCache},
        proof_type::ProofType,
    };

    /// A proof is claimed only while it is neither proven nor in progress.
    #[test]
    fn test_proof_claimed_once() {
        let cache = ProofCache::new(1);
        let claim = || cache.claim(B256::ZERO, ProofType::RethZisk, true);
        let claim_without_witness = || cache.claim(B256::ZERO, ProofType::RethZisk, false);

        assert!(matches!(claim_without_witness(), ClaimOutcome::NoWitness));
        let ClaimOutcome::New(first) = claim() else {
            panic!("the first request claims the proof")
        };
        assert!(matches!(claim(), ClaimOutcome::Claimed));
        assert!(matches!(claim_without_witness(), ClaimOutcome::Claimed));
        drop(first);
        let ClaimOutcome::New(second) = claim() else {
            panic!("a failed proof is claimed again")
        };
        second.complete(Bytes::from_static(&[1]));
        assert!(matches!(claim(), ClaimOutcome::Proven(proof) if proof[..] == [1]));
        assert!(matches!(claim_without_witness(), ClaimOutcome::Proven(_)));
        assert!(cache.0.lock().unwrap().claimed.is_empty());
    }
}
