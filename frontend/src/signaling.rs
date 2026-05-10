//! Subscribe to + publish into the WebRTC signaling contract over the
//! local freenet node's WebSocket API.
//!
//! Wire flow:
//!   GET → contract returns full `ContractState` (every publisher's
//!     last signed snapshot)
//!   SUBSCRIBE → node pushes `UpdateNotification` deltas (one
//!     publisher's new snapshot at a time)
//!   UPDATE → we publish our own signed `SignalPayload` so the other
//!     side sees our presence + outbox
//!
//! Publishing is rate-limited / debounced by the caller — every change
//! to our outbox bumps the timestamp_ms and signs a fresh payload.

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;

use ed25519_dalek::{Signer, SigningKey};
use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractRequest, ContractResponse, Error as WebApiError,
    HostResponse,
};
use freenet_stdlib::prelude::{
    CodeHash, ContractInstanceId, ContractKey, StateDelta, UpdateData,
};
use shared::{
    CanvasStroke, ContractDelta, ContractState, DirectedSignal, SignalPayload, SignedEntry,
};
use yew::Callback;

use crate::ws_shim::WsShim;

#[derive(Clone, Debug, PartialEq)]
pub enum SignalingStatus {
    Connecting,
    Subscribed,
    Error(String),
}

/// One publisher's verified snapshot, handed to the app.
#[derive(Clone, Debug)]
pub struct RemoteSnapshot {
    pub payload: SignalPayload,
}

pub struct SignalingClient {
    api: Rc<RefCell<Option<WsShim>>>,
    contract_key: ContractKey,
}

impl SignalingClient {
    /// Open WS, subscribe + initial Get. Returns immediately; status
    /// transitions arrive via `on_status`, snapshots via `on_snapshot`.
    ///
    /// `node_ws_url`: e.g. `ws://127.0.0.1:7509`
    /// `instance_id`: base58 contract instance id (needed for Get + Subscribe)
    /// `code_hash`: base58 WASM code hash (needed for Update — see
    ///   `ContractRequest::Update { key: ContractKey, .. }`)
    pub fn start(
        node_ws_url: &str,
        instance_id: &str,
        code_hash: &str,
        on_snapshot: Callback<Vec<RemoteSnapshot>>,
        on_status: Callback<SignalingStatus>,
    ) -> Result<Self, String> {
        let instance = ContractInstanceId::from_str(instance_id.trim())
            .map_err(|e| format!("bad instance_id: {e}"))?;
        let code_hash_bytes = bs58::decode(code_hash.trim())
            .into_vec()
            .map_err(|e| format!("bad code_hash base58: {e}"))?;
        let code_hash_arr: [u8; 32] = code_hash_bytes
            .as_slice()
            .try_into()
            .map_err(|_| "code_hash must be 32 bytes".to_string())?;
        let contract_key = ContractKey::from_id_and_code(instance, CodeHash::from(&code_hash_arr));

        let url = format!(
            "{}/v1/contract/command?encodingProtocol=native",
            node_ws_url.trim_end_matches('/')
        );

        on_status.emit(SignalingStatus::Connecting);

        let socket = web_sys::WebSocket::new(&url)
            .map_err(|e| format!("WebSocket::new failed: {e:?}"))?;
        let api: Rc<RefCell<Option<WsShim>>> = Rc::new(RefCell::new(None));

        let result_handler = {
            let on_snapshot = on_snapshot.clone();
            let on_status = on_status.clone();
            move |res: Result<HostResponse, ClientError>| {
                handle_response(res, &on_snapshot, &on_status);
            }
        };
        let error_handler = {
            let on_status = on_status.clone();
            move |e: WebApiError| {
                on_status.emit(SignalingStatus::Error(format!("{e:?}")));
            }
        };
        let onopen_handler = {
            let api = api.clone();
            let on_status = on_status.clone();
            let instance = instance;
            move || {
                let api = api.clone();
                let on_status = on_status.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let mut guard = api.borrow_mut();
                    let Some(api) = guard.as_mut() else { return; };
                    let sub = ClientRequest::ContractOp(ContractRequest::Subscribe {
                        key: instance,
                        summary: None,
                    });
                    if let Err(e) = api.send(sub).await {
                        on_status.emit(SignalingStatus::Error(format!("subscribe: {e:?}")));
                        return;
                    }
                    let get = ClientRequest::ContractOp(ContractRequest::Get {
                        key: instance,
                        return_contract_code: false,
                        subscribe: false,
                        blocking_subscribe: false,
                    });
                    if let Err(e) = api.send(get).await {
                        web_sys::console::warn_1(&format!("initial Get send: {e:?}").into());
                    }
                });
            }
        };

        let started = WsShim::start(socket, result_handler, error_handler, onopen_handler);
        *api.borrow_mut() = Some(started);

        Ok(Self { api, contract_key })
    }

    /// Build a fresh signed `SignalPayload`, wrap it as a single-entry
    /// `ContractDelta`, and ship it as `ContractRequest::Update`.
    pub async fn publish(
        &self,
        signing_key: &SigningKey,
        display_name: &str,
        outbox: Vec<DirectedSignal>,
        canvas_strokes: Vec<CanvasStroke>,
    ) -> Result<(), String> {
        let now_ms = (js_sys::Date::now() as u64).max(1);
        let payload = SignalPayload {
            public_key: signing_key.verifying_key().to_bytes(),
            display_name: display_name.chars().take(64).collect(),
            timestamp_ms: now_ms,
            outbox,
            canvas_strokes,
        };
        let bytes = bincode::serialize(&payload).map_err(|e| format!("serialize: {e}"))?;
        let sig: ed25519_dalek::Signature = signing_key.sign(&bytes);
        let signed = SignedEntry { payload: bytes, signature: sig.to_bytes() };
        let delta = ContractDelta { entries: vec![signed] };
        let delta_bytes = bincode::serialize(&delta).map_err(|e| format!("serialize delta: {e}"))?;

        let mut guard = self.api.borrow_mut();
        let Some(api) = guard.as_mut() else {
            return Err("ws not open".into());
        };
        let req = ClientRequest::ContractOp(ContractRequest::Update {
            key: self.contract_key,
            data: UpdateData::Delta(StateDelta::from(delta_bytes)),
        });
        api.send(req).await.map_err(|e| format!("send: {e:?}"))?;
        Ok(())
    }
}

fn handle_response(
    res: Result<HostResponse, ClientError>,
    on_snapshot: &Callback<Vec<RemoteSnapshot>>,
    on_status: &Callback<SignalingStatus>,
) {
    let resp = match res {
        Ok(r) => r,
        Err(e) => {
            on_status.emit(SignalingStatus::Error(format!("{e}")));
            return;
        }
    };
    match resp {
        HostResponse::ContractResponse(ContractResponse::SubscribeResponse { .. }) => {
            on_status.emit(SignalingStatus::Subscribed);
        }
        HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }) => {
            if let Some(snaps) = decode_state_or_delta(state.as_ref()) {
                on_snapshot.emit(snaps);
            }
            on_status.emit(SignalingStatus::Subscribed);
        }
        HostResponse::ContractResponse(ContractResponse::UpdateNotification { update, .. }) => {
            for bytes in collect_update_bytes(&update) {
                if let Some(snaps) = decode_state_or_delta(&bytes) {
                    on_snapshot.emit(snaps);
                }
            }
            on_status.emit(SignalingStatus::Subscribed);
        }
        HostResponse::ContractResponse(ContractResponse::PutResponse { .. }) => {}
        _ => {}
    }
}

fn collect_update_bytes(update: &UpdateData<'_>) -> Vec<Vec<u8>> {
    match update {
        UpdateData::Delta(d) => vec![d.as_ref().to_vec()],
        UpdateData::State(s) => vec![s.as_ref().to_vec()],
        UpdateData::StateAndDelta { state, delta } => {
            vec![state.as_ref().to_vec(), delta.as_ref().to_vec()]
        }
        UpdateData::RelatedDelta { delta, .. } => vec![delta.as_ref().to_vec()],
        UpdateData::RelatedState { state, .. } => vec![state.as_ref().to_vec()],
        UpdateData::RelatedStateAndDelta { state, delta, .. } => {
            vec![state.as_ref().to_vec(), delta.as_ref().to_vec()]
        }
        _ => vec![],
    }
}

fn decode_state_or_delta(bytes: &[u8]) -> Option<Vec<RemoteSnapshot>> {
    if let Ok(delta) = bincode::deserialize::<ContractDelta>(bytes) {
        return Some(verify_all(delta.entries));
    }
    if let Ok(state) = bincode::deserialize::<ContractState>(bytes) {
        return Some(verify_all(state.entries.into_values().collect()));
    }
    None
}

fn verify_all(entries: Vec<SignedEntry>) -> Vec<RemoteSnapshot> {
    entries
        .into_iter()
        .filter_map(|e| e.verify().ok().map(|payload| RemoteSnapshot { payload }))
        .collect()
}
