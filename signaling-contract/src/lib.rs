//! WebRTC signaling contract — same shape as the topology contract:
//! one signed snapshot per Ed25519 publisher, LWW by timestamp_ms.
//! Payload is a `SignalPayload` carrying presence + outbox of directed
//! WebRTC signals (offer/answer/ICE).
//!
//! The contract prunes entries that fall more than `MAX_STALE_MS`
//! behind the freshest entry on every update, so closed tabs and
//! disconnected peers don't accumulate forever. Pivoting on the
//! state's max timestamp (not a wall clock) keeps pruning
//! deterministic across replicas.

use freenet_stdlib::prelude::*;
use shared::{ContractDelta, ContractState, ContractSummary};

/// 5 minutes — comfortably > the 15s frontend heartbeat × several
/// missed beats, so a brief network blip doesn't drop a live peer.
const MAX_STALE_MS: u64 = 5 * 60 * 1000;

#[allow(dead_code)]
struct Signaling;

#[contract]
impl ContractInterface for Signaling {
    fn validate_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        let parsed: ContractState = match bincode::deserialize(state.as_ref()) {
            Ok(s) => s,
            Err(_) => return Ok(ValidateResult::Invalid),
        };
        for entry in parsed.entries.values() {
            if entry.verify().is_err() {
                return Ok(ValidateResult::Invalid);
            }
        }
        Ok(ValidateResult::Valid)
    }

    fn update_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let mut current: ContractState = bincode::deserialize(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        for update in data {
            match update {
                UpdateData::Delta(d) => apply_delta_bytes(&mut current, d.as_ref())?,
                UpdateData::State(s) => {
                    let incoming: ContractState = bincode::deserialize(s.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    for (_, entry) in incoming.entries {
                        let _ = current.apply(entry);
                    }
                }
                UpdateData::StateAndDelta { state: s, delta: d } => {
                    let incoming: ContractState = bincode::deserialize(s.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    for (_, entry) in incoming.entries {
                        let _ = current.apply(entry);
                    }
                    apply_delta_bytes(&mut current, d.as_ref())?;
                }
                _ => {}
            }
        }

        // Prune entries whose timestamp is more than MAX_STALE_MS
        // behind the freshest. Runs on every update so the state
        // stays bounded even with high churn.
        current.prune_stale(MAX_STALE_MS);

        let bytes =
            bincode::serialize(&current).map_err(|e| ContractError::Other(e.to_string()))?;
        Ok(UpdateModification::valid(State::from(bytes)))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        let current: ContractState = bincode::deserialize(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let bytes = bincode::serialize(&current.summarize())
            .map_err(|e| ContractError::Other(e.to_string()))?;
        Ok(StateSummary::from(bytes))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let current: ContractState = bincode::deserialize(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary: ContractSummary = bincode::deserialize(summary.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let bytes = bincode::serialize(&current.delta_against(&summary))
            .map_err(|e| ContractError::Other(e.to_string()))?;
        Ok(StateDelta::from(bytes))
    }
}

#[allow(dead_code)]
fn apply_delta_bytes(state: &mut ContractState, bytes: &[u8]) -> Result<(), ContractError> {
    let delta: ContractDelta =
        bincode::deserialize(bytes).map_err(|e| ContractError::Deser(e.to_string()))?;
    for entry in delta.entries {
        let _ = state.apply(entry);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use shared::{DirectedSignal, SignalKind, SignalPayload, SignedEntry};

    fn sign_entry(sk: &SigningKey, ts: u64, outbox: Vec<DirectedSignal>) -> SignedEntry {
        let payload = SignalPayload {
            public_key: sk.verifying_key().to_bytes(),
            display_name: "test".into(),
            timestamp_ms: ts,
            outbox,
            canvas_strokes: vec![],
        };
        let bytes = bincode::serialize(&payload).unwrap();
        let sig: ed25519_dalek::Signature = sk.sign(&bytes);
        SignedEntry { payload: bytes, signature: sig.to_bytes() }
    }

    #[test]
    fn empty_state_validates() {
        let empty = bincode::serialize(&ContractState::default()).unwrap();
        let res = Signaling::validate_state(
            Parameters::from(vec![]),
            State::from(empty),
            RelatedContracts::default(),
        )
        .unwrap();
        assert_eq!(res, ValidateResult::Valid);
    }

    #[test]
    fn offer_round_trips_through_delta() {
        let sk_alice = SigningKey::from_bytes(&[1u8; 32]);
        let sk_bob = SigningKey::from_bytes(&[2u8; 32]);
        let bob_pk = sk_bob.verifying_key().to_bytes();

        let entry = sign_entry(
            &sk_alice,
            42,
            vec![DirectedSignal {
                signal_id: 1,
                target: bob_pk,
                kind: SignalKind::Offer { sdp: "v=0\r\n...".into() },
            }],
        );
        let delta = ContractDelta { entries: vec![entry] };
        let delta_bytes = bincode::serialize(&delta).unwrap();
        let initial = bincode::serialize(&ContractState::default()).unwrap();

        let m = Signaling::update_state(
            Parameters::from(vec![]),
            State::from(initial),
            vec![UpdateData::Delta(StateDelta::from(delta_bytes))],
        )
        .unwrap();

        let new_state: ContractState = bincode::deserialize(m.new_state.unwrap().as_ref()).unwrap();
        let alice_entry = new_state
            .entries
            .get(&sk_alice.verifying_key().to_bytes())
            .unwrap();
        let p: SignalPayload = bincode::deserialize(&alice_entry.payload).unwrap();
        assert_eq!(p.outbox.len(), 1);
        assert_eq!(p.outbox[0].target, bob_pk);
    }

    #[test]
    fn update_drops_stale_publishers() {
        // Two old publishers + one fresh publish that lands far enough
        // ahead to trip the prune. After update_state runs, only the
        // entry within MAX_STALE_MS of the freshest survives.
        let sk_old1 = SigningKey::from_bytes(&[1u8; 32]);
        let sk_old2 = SigningKey::from_bytes(&[2u8; 32]);
        let sk_fresh = SigningKey::from_bytes(&[3u8; 32]);

        let mut prior = ContractState::default();
        prior
            .apply(sign_entry(&sk_old1, 1_000, vec![]))
            .unwrap();
        prior
            .apply(sign_entry(&sk_old2, 2_000, vec![]))
            .unwrap();
        let initial = bincode::serialize(&prior).unwrap();

        // Fresh entry sits MAX_STALE_MS + 10s past the old ones.
        let fresh_ts = 2_000 + MAX_STALE_MS + 10_000;
        let delta = ContractDelta {
            entries: vec![sign_entry(&sk_fresh, fresh_ts, vec![])],
        };
        let delta_bytes = bincode::serialize(&delta).unwrap();

        let m = Signaling::update_state(
            Parameters::from(vec![]),
            State::from(initial),
            vec![UpdateData::Delta(StateDelta::from(delta_bytes))],
        )
        .unwrap();

        let new_state: ContractState =
            bincode::deserialize(m.new_state.unwrap().as_ref()).unwrap();
        assert_eq!(new_state.entries.len(), 1);
        assert!(new_state
            .entries
            .contains_key(&sk_fresh.verifying_key().to_bytes()));
        // The two stale entries are gone.
        assert!(!new_state
            .entries
            .contains_key(&sk_old1.verifying_key().to_bytes()));
        assert!(!new_state
            .entries
            .contains_key(&sk_old2.verifying_key().to_bytes()));
    }

    #[test]
    fn delta_order_independence() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let entries: Vec<SignedEntry> = [50u64, 200, 100, 300, 250]
            .iter()
            .map(|ts| sign_entry(&sk, *ts, vec![]))
            .collect();

        let mut a = ContractState::default();
        for e in &entries {
            a.apply(e.clone()).unwrap();
        }
        let mut b = ContractState::default();
        let mut rev = entries.clone();
        rev.reverse();
        for e in rev {
            b.apply(e).unwrap();
        }
        assert_eq!(a, b);
    }
}
