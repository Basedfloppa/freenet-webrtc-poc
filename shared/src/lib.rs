//! Wire types for the WebRTC-over-Freenet PoC.
//!
//! Contract state is one signed snapshot per publisher (Ed25519 pubkey),
//! Last-Writer-Wins by `timestamp_ms`. Each snapshot carries:
//!   - presence (display name + ts) so peers can list each other,
//!   - an outbox of directed signals (offer/answer/ICE) targeted at
//!     specific other pubkeys.
//!
//! WebRTC handshake flow:
//!   A publishes presence
//!   B publishes presence
//!   A clicks "call B" → A puts `Offer{ to: B, sdp }` in its outbox + republishes
//!   B's subscriber sees A's outbox, processes Offer → creates RTCPeerConnection,
//!     puts `Answer{ to: A, sdp }` in its outbox + republishes
//!   Both sides keep republishing accumulated `IceCandidate{ to: X, ... }` entries
//!     until ICE gathering completes
//!   Once direct WebRTC channel is up, signals can be cleared from outbox
//!
//! Receivers dedupe directed signals by `signal_id` (publisher-monotonic).

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

pub const PUBKEY_LEN: usize = 32;
pub const SIG_LEN: usize = 64;

/// One signaling message addressed to a specific peer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectedSignal {
    /// Monotonic per-publisher; receivers dedupe on (sender_pubkey, signal_id).
    pub signal_id: u64,
    #[serde(with = "byte_array_32")]
    pub target: [u8; PUBKEY_LEN],
    pub kind: SignalKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SignalKind {
    /// Caller-side SDP offer.
    Offer { sdp: String },
    /// Callee-side SDP answer.
    Answer { sdp: String },
    /// Trickle ICE candidate. `candidate` may be empty string to mark
    /// end-of-candidates per spec; receivers should pass through.
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u16>,
    },
    /// Politely tell the other side we're hanging up so they tear down
    /// their RTCPeerConnection without waiting for ICE timeout.
    Hangup,
}

/// One pen stroke contributed by a publisher to the shared canvas.
/// Owner is implicit — it's the publisher whose `SignalPayload`
/// carries this struct, so the contract verifies authorship for free.
/// Coordinates are fractions of canvas size in `[0, 1]`.
///
/// Shape encodes the stroke kind:
///   - `points.len() >= 2` → polyline (pen)
///   - `points.len() == 1` → dot (single click)
///   - `points.is_empty()` → full-canvas fill (paint bucket)
/// `width` is in CSS px and ignored for the fill case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanvasStroke {
    pub color: String,
    pub width: f32,
    pub points: Vec<(f32, f32)>,
}

/// What each publisher signs and republishes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignalPayload {
    #[serde(with = "byte_array_32")]
    pub public_key: [u8; PUBKEY_LEN],
    /// Free-form display name; truncated to 64 bytes by the contract.
    pub display_name: String,
    /// Wall-clock at publisher (ms since UNIX epoch). LWW key.
    pub timestamp_ms: u64,
    /// Currently-active outgoing signals. Cleared when no longer needed
    /// (e.g. after the WebRTC connection is established).
    pub outbox: Vec<DirectedSignal>,
    /// Persistent canvas contribution from this publisher. Snapshot —
    /// LWW per pubkey, so the latest publish replaces the prior list.
    pub canvas_strokes: Vec<CanvasStroke>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignedEntry {
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    #[serde(with = "byte_array_64")]
    pub signature: [u8; SIG_LEN],
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContractState {
    pub entries: BTreeMap<[u8; PUBKEY_LEN], SignedEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContractDelta {
    pub entries: Vec<SignedEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContractSummary {
    pub known_timestamps: BTreeMap<[u8; PUBKEY_LEN], u64>,
}

#[derive(Debug, PartialEq)]
pub enum VerifyError {
    BadPayload,
    BadSignature,
    KeyMismatch,
}

impl SignedEntry {
    pub fn verify(&self) -> Result<SignalPayload, VerifyError> {
        let payload: SignalPayload =
            bincode::deserialize(&self.payload).map_err(|_| VerifyError::BadPayload)?;
        let vk = VerifyingKey::from_bytes(&payload.public_key)
            .map_err(|_| VerifyError::KeyMismatch)?;
        let sig = Signature::from_bytes(&self.signature);
        vk.verify(&self.payload, &sig)
            .map_err(|_| VerifyError::BadSignature)?;
        Ok(payload)
    }
}

impl ContractState {
    /// LWW per pubkey by timestamp_ms. Returns true if state changed.
    pub fn apply(&mut self, entry: SignedEntry) -> Result<bool, VerifyError> {
        let payload = entry.verify()?;
        let key = payload.public_key;
        if let Some(existing) = self.entries.get(&key) {
            if let Ok(existing_payload) = bincode::deserialize::<SignalPayload>(&existing.payload) {
                if existing_payload.timestamp_ms >= payload.timestamp_ms {
                    return Ok(false);
                }
            }
        }
        self.entries.insert(key, entry);
        Ok(true)
    }

    pub fn summarize(&self) -> ContractSummary {
        let mut s = ContractSummary::default();
        for (k, v) in &self.entries {
            if let Ok(p) = bincode::deserialize::<SignalPayload>(&v.payload) {
                s.known_timestamps.insert(*k, p.timestamp_ms);
            }
        }
        s
    }

    /// Drop entries whose timestamp falls more than `max_age_ms`
    /// behind the freshest timestamp in the state. Returns true if
    /// anything was removed.
    ///
    /// Why pivot on the freshest entry instead of a wall clock: a
    /// contract has no access to the host clock and must be
    /// deterministic across replicas. Using `max(all timestamps)` as
    /// the reference point gives the same answer on every node and
    /// still prunes naturally as fresh updates flow in. A room with
    /// no activity at all retains everything (max doesn't advance);
    /// the moment any peer publishes, anyone whose last publish was
    /// > `max_age_ms` behind that one is dropped.
    pub fn prune_stale(&mut self, max_age_ms: u64) -> bool {
        let mut newest: u64 = 0;
        for entry in self.entries.values() {
            if let Ok(p) = bincode::deserialize::<SignalPayload>(&entry.payload) {
                if p.timestamp_ms > newest {
                    newest = p.timestamp_ms;
                }
            }
        }
        if newest == 0 {
            return false;
        }
        let cutoff = newest.saturating_sub(max_age_ms);
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            match bincode::deserialize::<SignalPayload>(&entry.payload) {
                Ok(p) => p.timestamp_ms >= cutoff,
                // Unparseable entry — drop. validate_state should
                // already have rejected this so it's defensive.
                Err(_) => false,
            }
        });
        self.entries.len() != before
    }

    pub fn delta_against(&self, summary: &ContractSummary) -> ContractDelta {
        let mut entries = Vec::new();
        for (k, v) in &self.entries {
            let payload = match bincode::deserialize::<SignalPayload>(&v.payload) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let known = summary.known_timestamps.get(k).copied().unwrap_or(0);
            if payload.timestamp_ms > known {
                entries.push(v.clone());
            }
        }
        ContractDelta { entries }
    }
}

mod byte_array_32 {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod byte_array_64 {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 64 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn make_entry(sk: &SigningKey, ts: u64, outbox: Vec<DirectedSignal>) -> SignedEntry {
        let payload = SignalPayload {
            public_key: sk.verifying_key().to_bytes(),
            display_name: "alice".into(),
            timestamp_ms: ts,
            outbox,
            canvas_strokes: vec![],
        };
        let bytes = bincode::serialize(&payload).unwrap();
        let sig: ed25519_dalek::Signature = sk.sign(&bytes);
        SignedEntry {
            payload: bytes,
            signature: sig.to_bytes(),
        }
    }

    #[test]
    fn signed_entry_verifies() {
        let sk = SigningKey::generate(&mut OsRng);
        let entry = make_entry(&sk, 1, vec![]);
        let p = entry.verify().unwrap();
        assert_eq!(p.public_key, sk.verifying_key().to_bytes());
        assert_eq!(p.timestamp_ms, 1);
    }

    #[test]
    fn lww_keeps_newer_outbox() {
        let sk = SigningKey::generate(&mut OsRng);
        let mut state = ContractState::default();

        let target = [9u8; 32];
        let with_offer = make_entry(
            &sk,
            100,
            vec![DirectedSignal {
                signal_id: 1,
                target,
                kind: SignalKind::Offer { sdp: "v=0...".into() },
            }],
        );
        let cleared = make_entry(&sk, 200, vec![]);

        assert!(state.apply(with_offer).unwrap());
        assert!(state.apply(cleared).unwrap());

        let stored = state.entries.get(&sk.verifying_key().to_bytes()).unwrap();
        let p: SignalPayload = bincode::deserialize(&stored.payload).unwrap();
        assert_eq!(p.timestamp_ms, 200);
        assert!(p.outbox.is_empty());
    }

    #[test]
    fn canvas_strokes_round_trip_through_lww() {
        // Verifies a publisher can replace their stroke list by
        // republishing — receivers see the *latest* set, not a merge.
        // This is the contract that the frontend relies on for "my
        // canvas state == my latest publish, full snapshot".
        let sk = SigningKey::generate(&mut OsRng);
        let mut state = ContractState::default();

        let mk = |ts: u64, n_strokes: usize| -> SignedEntry {
            let strokes: Vec<CanvasStroke> = (0..n_strokes)
                .map(|i| CanvasStroke {
                    color: format!("#{:06x}", i * 0x111111),
                    width: 4.0,
                    points: vec![(i as f32 * 0.1, 0.5), (i as f32 * 0.1 + 0.05, 0.6)],
                })
                .collect();
            let payload = SignalPayload {
                public_key: sk.verifying_key().to_bytes(),
                display_name: "p".into(),
                timestamp_ms: ts,
                outbox: vec![],
                canvas_strokes: strokes,
            };
            let bytes = bincode::serialize(&payload).unwrap();
            let sig: ed25519_dalek::Signature = sk.sign(&bytes);
            SignedEntry { payload: bytes, signature: sig.to_bytes() }
        };

        assert!(state.apply(mk(10, 3)).unwrap());
        assert!(state.apply(mk(20, 5)).unwrap());
        // Older snapshot ignored even though stroke count differs.
        assert!(!state.apply(mk(15, 99)).unwrap());

        let stored = state.entries.get(&sk.verifying_key().to_bytes()).unwrap();
        let p: SignalPayload = bincode::deserialize(&stored.payload).unwrap();
        assert_eq!(p.canvas_strokes.len(), 5);
        assert_eq!(p.canvas_strokes[4].color, "#444444");
    }

    #[test]
    fn prune_stale_drops_entries_far_behind_freshest() {
        // With three publishers ranging from ts=100 to ts=1100,
        // pruning with max_age=500 keeps everything within 500ms of
        // the freshest (1100) — i.e. ts >= 600. The 100 and 200
        // entries are dropped; the 1100 entry stays.
        let sk_a = SigningKey::generate(&mut OsRng);
        let sk_b = SigningKey::generate(&mut OsRng);
        let sk_c = SigningKey::generate(&mut OsRng);
        let mut state = ContractState::default();
        state.apply(make_entry(&sk_a, 100, vec![])).unwrap();
        state.apply(make_entry(&sk_b, 200, vec![])).unwrap();
        state.apply(make_entry(&sk_c, 1100, vec![])).unwrap();

        let removed = state.prune_stale(500);
        assert!(removed);
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.contains_key(&sk_c.verifying_key().to_bytes()));
    }

    #[test]
    fn prune_stale_noop_on_empty_or_recent() {
        let mut empty = ContractState::default();
        assert!(!empty.prune_stale(1000));

        let sk = SigningKey::generate(&mut OsRng);
        let mut state = ContractState::default();
        state.apply(make_entry(&sk, 1000, vec![])).unwrap();
        // Single entry can never be stale relative to itself.
        assert!(!state.prune_stale(100));
        assert_eq!(state.entries.len(), 1);
    }

    #[test]
    fn cross_signing_blocked() {
        let sk_a = SigningKey::generate(&mut OsRng);
        let sk_b = SigningKey::generate(&mut OsRng);
        let payload = SignalPayload {
            public_key: sk_b.verifying_key().to_bytes(),
            display_name: "impostor".into(),
            timestamp_ms: 1,
            outbox: vec![],
            canvas_strokes: vec![],
        };
        let bytes = bincode::serialize(&payload).unwrap();
        let sig: ed25519_dalek::Signature = sk_a.sign(&bytes);
        let entry = SignedEntry { payload: bytes, signature: sig.to_bytes() };
        assert_eq!(entry.verify(), Err(VerifyError::BadSignature));
    }
}
