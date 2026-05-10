//! WebRTC-over-Freenet PoC frontend — collaborative drawing variant.
//!
//! Browser tabs (different localStorage = different identities) pointing
//! at the same signaling contract can:
//!   1. See each other in the peer list (via subscribed contract state).
//!   2. Establish direct WebRTC datachannels (mesh: many parallel calls).
//!   3. Draw on a shared canvas — strokes flow over the channel for
//!      low-latency display, then are persisted in the contract on
//!      stroke-end so late-joiners catch up automatically.
//!
//! Why a datachannel app and not media: `navigator.mediaDevices` is
//! `undefined` inside freenet's null-origin sandbox iframe (spec
//! requires "secure context"). RTCDataChannel has no such restriction,
//! so we sidestep the blocker. The drawing pipeline still exercises
//! every interesting WebRTC code path (offer/answer, trickle ICE, NAT
//! traversal, datachannel) without touching MediaDevices.
//!
//! Wire protocol (text frames over datachannel, comma-separated):
//!   `b,<color_hex6>,<x>,<y>` — begin stroke at fractional point
//!   `m,<x>,<y>`              — extend current stroke
//!   `e`                      — end current stroke
//!   `c`                      — clear all strokes by *the sender* only
//! Coordinates are fractions of canvas size in [0,1] so both sides
//! render at the same logical position regardless of pixel dims.
//! Stroke ownership is implicit: anything received on the channel from
//! peer R is attributed to R (no per-frame author field needed).
//!
//! Persistence: when a local stroke ends, we republish the contract
//! entry with our full stroke list as `canvas_strokes`. Receivers'
//! snapshot handler replaces our prior entry's strokes with the new
//! list (LWW). New peers joining the room see the contract snapshot
//! and reconstruct everyone's contributions before any datachannel.

mod peer;
mod signaling;
mod ws_shim;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use shared::{CanvasStroke, DirectedSignal, SignalKind};
use wasm_bindgen::{closure::Closure, JsCast};
use wasm_bindgen_futures::spawn_local;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlInputElement};
use yew::prelude::*;

use peer::{IceCandidate, OpenHandler, Peer, RemoteMsgHandler};
use signaling::{RemoteSnapshot, SignalingClient, SignalingStatus};

const STORAGE_KEY_SK: &str = "webrtc-poc:sk-v1";
const STORAGE_KEY_NAME: &str = "webrtc-poc:name-v1";
const STORAGE_KEY_WS: &str = "webrtc-poc:ws-v1";
const STORAGE_KEY_INSTANCE: &str = "webrtc-poc:instance-v1";
const STORAGE_KEY_CODE_HASH: &str = "webrtc-poc:code-hash-v1";
const STORAGE_KEY_COLOR: &str = "webrtc-poc:color-v1";
const STORAGE_KEY_WIDTH: &str = "webrtc-poc:width-v1";

/// Quick-pick palette: 12 colors covering the visible spectrum plus
/// neutral whites/grays/black for outlines and shading. Visible on
/// the dark canvas; chosen so they're easy to tell apart at a glance.
const COLOR_PALETTE: &[&str] = &[
    "#ff5959", "#ffa959", "#ffd859", "#5acf6c", "#5acfa6", "#5ab8ff",
    "#6c8aff", "#b46cff", "#ff5fc2", "#ffffff", "#a0a8b4", "#1a1f27",
];

/// Canvas background. The eraser tool draws strokes in this exact
/// color so they visually "erase" pixels behind them — no special
/// composite mode needed, the renderer treats them like any pen
/// stroke and they cleanly overwrite previous strokes.
const CANVAS_BG: &str = "#06090d";

const DEFAULT_PEN_WIDTH: f32 = 4.0;
const MIN_BRUSH_WIDTH: f32 = 1.0;
const MAX_BRUSH_WIDTH: f32 = 40.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tool {
    Pen,
    Eraser,
    Fill,
}

const DEFAULT_WS: &str = "ws://127.0.0.1:7509";
/// Pre-published signaling contract. Random parameters → unique
/// instance_id, security-by-obscurity. Code is open and re-publishable.
/// Updated when the contract WASM rebuilds (new code_hash → new instance).
const DEFAULT_INSTANCE_ID: &str = "CVXFNq3Qkddupfpn1mCT597pYy254Y2Zvv7woK9ar6cm";
const DEFAULT_CODE_HASH: &str = "3PYNPAX8Mgic9KUG5yVncJuBY3oJWW57vswMy4QZhWsF";

/// Project source. Linked from the sidebar footer; clicking it opens
/// the repo in a new tab. The Freenet shell grants the iframe
/// `allow-popups` so `target=_blank` actually produces a new tab —
/// otherwise sandbox would silently swallow the navigation.
const REPO_URL: &str = "https://github.com/Basedfloppa/freenet-webrtc-poc";

/// How often to republish our presence so peers know we're alive.
const HEARTBEAT_MS: u32 = 15_000;
/// How often to recompute the online-indicator (cheap, no network).
const ONLINE_TICK_MS: u32 = 5_000;
/// A peer is considered online if their snapshot timestamp is within
/// this window of `Date.now()`. Should be > 2× `HEARTBEAT_MS` so a
/// single missed heartbeat doesn't flip the indicator.
const ONLINE_WINDOW_MS: u64 = 45_000;

#[derive(Clone, Debug, PartialEq)]
struct PeerView {
    display_name: String,
    last_seen_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum LogLevel {
    Info,
    Warn,
    Err,
}

#[derive(Clone, Debug, PartialEq)]
struct LogLine {
    level: LogLevel,
    text: String,
}

/// One pen stroke. Owner is the publisher who drew it (Ed25519 pubkey).
/// Points are canvas-relative fractions in [0,1]. Empty `points` =
/// fill stroke (paints the entire canvas in `color`, ignoring `width`).
#[derive(Clone, Debug)]
struct Stroke {
    owner: [u8; 32],
    color: String,
    width: f32,
    points: Vec<(f32, f32)>,
    /// Transient flag for in-flight eraser strokes. Never committed
    /// to `strokes` and never serialized into `canvas_strokes` — the
    /// eraser's job is to *remove* strokes via hit-test on mouseup,
    /// not to add a bg-coloured stroke that would also clobber the
    /// grid drawn below.
    is_eraser: bool,
}

#[function_component(App)]
fn app() -> Html {
    // -- Identity ------------------------------------------------------
    // `load_or_generate_sk` reads URL-hash first (sandbox-safe), falls
    // back to localStorage, then generates fresh + writes to both. The
    // optional name is the user's prior alias from the same source.
    // We resolve once and store sk + initial-name in stable state slots.
    let identity = use_state(|| Rc::new(load_or_generate_sk()));
    let signing_key = use_state(|| Rc::new(identity.0.clone()));
    let my_pk_hex = hex::encode(signing_key.verifying_key().to_bytes());
    let my_pk: [u8; 32] = signing_key.verifying_key().to_bytes();

    // -- Input fields (persisted to localStorage) ----------------------
    let display_name = use_state(|| {
        identity
            .1
            .clone()
            .or_else(|| read_storage(STORAGE_KEY_NAME))
            .unwrap_or_else(default_name)
    });
    // Stable mirror of `display_name` for closures captured once on
    // mount (heartbeat). UseStateHandles snapshot the value at the
    // render they were created in — without this cell, a long-lived
    // captured handle keeps publishing the *initial* name forever
    // even after the user types a new one. We sync state→cell on
    // every render below.
    let display_name_cell: Rc<RefCell<String>> = use_mut_ref(|| (*display_name).clone());
    *display_name_cell.borrow_mut() = (*display_name).clone();
    let ws_url = use_state(|| read_storage(STORAGE_KEY_WS).unwrap_or_else(default_ws_url));
    let instance_id = use_state(|| {
        read_storage(STORAGE_KEY_INSTANCE).unwrap_or_else(|| DEFAULT_INSTANCE_ID.into())
    });
    let code_hash = use_state(|| {
        read_storage(STORAGE_KEY_CODE_HASH).unwrap_or_else(|| DEFAULT_CODE_HASH.into())
    });
    let pen_color = use_state(|| {
        read_storage(STORAGE_KEY_COLOR).unwrap_or_else(|| color_from_pk(&my_pk))
    });
    let pen_width = use_state(|| {
        read_storage(STORAGE_KEY_WIDTH)
            .and_then(|s| s.parse::<f32>().ok())
            .map(|w| w.clamp(MIN_BRUSH_WIDTH, MAX_BRUSH_WIDTH))
            .unwrap_or(DEFAULT_PEN_WIDTH)
    });
    let current_tool = use_state(|| Tool::Pen);

    // -- Cross-callback shared state -----------------------------------
    let signaling: Rc<RefCell<Option<SignalingClient>>> = use_mut_ref(|| None);
    // Mesh: one Peer per remote pubkey we've established a call with.
    // Mutated from many places (call, hangup, ICE, snapshot handler);
    // the inner HashMap holds Rc<Peer> for cheap clone-into-spawn_local.
    let peers: Rc<RefCell<HashMap<[u8; 32], Rc<Peer>>>> = use_mut_ref(HashMap::new);
    let outbox: Rc<RefCell<Vec<DirectedSignal>>> = use_mut_ref(Vec::new);
    let signal_counter: Rc<RefCell<u64>> = use_mut_ref(|| 0u64);
    let seen_signals: Rc<RefCell<HashMap<[u8; 32], u64>>> = use_mut_ref(HashMap::new);
    // Inbound offers waiting for the user to Accept / Reject. Stored
    // here (not auto-processed) so a remote peer can't open a
    // datachannel without consent. ICE candidates that race ahead of
    // the user's decision are buffered in `pending_ice` and replayed
    // into the freshly-built Peer once Accept fires.
    let pending_offers: Rc<RefCell<HashMap<[u8; 32], String>>> = use_mut_ref(HashMap::new);
    let pending_ice: Rc<RefCell<HashMap<[u8; 32], Vec<IceCandidate>>>> =
        use_mut_ref(HashMap::new);

    // Drawing state. Held in shared cells (not Yew state) so canvas
    // redraws driven by raw mouse / message events don't churn the
    // virtual DOM on every frame.
    let strokes: Rc<RefCell<Vec<Stroke>>> = use_mut_ref(Vec::new);
    let current_local: Rc<RefCell<Option<Stroke>>> = use_mut_ref(|| None);
    // Each remote peer can have an in-progress stroke at the same time
    // (mesh = N parallel pens). Keyed by remote pubkey.
    let current_remote: Rc<RefCell<HashMap<[u8; 32], Stroke>>> = use_mut_ref(HashMap::new);
    let is_drawing: Rc<RefCell<bool>> = use_mut_ref(|| false);

    // -- UI-driving state ----------------------------------------------
    let status = use_state(|| None::<SignalingStatus>);
    let peers_seen = use_state(HashMap::<[u8; 32], PeerView>::new);
    let connected_pks = use_state(HashSet::<[u8; 32]>::new);
    let log = use_state(Vec::<LogLine>::new);
    // UI mirror of `pending_offers` — Yew re-renders when this state
    // changes, while the Rc<RefCell> above holds the SDP for the
    // accept handler to consume.
    let pending_offer_view = use_state(HashMap::<[u8; 32], ()>::new);
    // Bumps every few seconds so the "online" pill on each peer-row
    // refreshes without waiting for a fresh contract snapshot.
    let now_tick = use_state(|| 0u32);

    let canvas_ref = use_node_ref();

    // ----------------------------------------------------------------
    //   Helpers
    // ----------------------------------------------------------------

    let push_log: Rc<dyn Fn(LogLevel, String)> = {
        let log = log.clone();
        Rc::new(move |level: LogLevel, text: String| {
            web_sys::console::log_1(&format!("[poc] {text}").into());
            let mut v = (*log).clone();
            v.push(LogLine { level, text });
            if v.len() > 200 {
                let drop = v.len() - 200;
                v.drain(0..drop);
            }
            log.set(v);
        })
    };

    // Canvas strokes are NEVER published to the contract — that would
    // make the drawing visible to every contract subscriber, even
    // people not in an active call with the author. We keep the field
    // in `SignalPayload` (always empty) for wire-format compatibility
    // and instead replay our own strokes peer-to-peer via the
    // datachannel `on_open` hook below.
    let my_canvas_strokes: Rc<dyn Fn() -> Vec<CanvasStroke>> =
        Rc::new(|| Vec::new());

    let republish: Rc<dyn Fn()> = {
        let signaling = signaling.clone();
        let signing_key = (*signing_key).clone();
        let display_name_cell = display_name_cell.clone();
        let outbox = outbox.clone();
        let push_log = push_log.clone();
        let my_canvas_strokes = my_canvas_strokes.clone();
        Rc::new(move || {
            let signaling = signaling.clone();
            let signing_key = signing_key.clone();
            let display_name = display_name_cell.borrow().clone();
            let outbox_snapshot = outbox.borrow().clone();
            let canvas_snapshot = my_canvas_strokes();
            let push_log = push_log.clone();
            spawn_local(async move {
                let guard = signaling.borrow();
                let Some(client) = guard.as_ref() else { return; };
                if let Err(e) = client
                    .publish(&signing_key, &display_name, outbox_snapshot, canvas_snapshot)
                    .await
                {
                    push_log(LogLevel::Warn, format!("publish failed: {e}"));
                }
            });
        })
    };

    let send_signal: Rc<dyn Fn([u8; 32], SignalKind)> = {
        let outbox = outbox.clone();
        let signal_counter = signal_counter.clone();
        let republish = republish.clone();
        Rc::new(move |target: [u8; 32], kind: SignalKind| {
            let id = {
                let mut c = signal_counter.borrow_mut();
                *c += 1;
                *c
            };
            outbox
                .borrow_mut()
                .push(DirectedSignal { signal_id: id, target, kind });
            republish();
        })
    };

    // Build a per-peer inbound-message handler. Closes over the remote
    // pubkey so received `b/m/e/c` frames are attributed to the right
    // owner without needing a per-frame author field.
    let make_remote_msg: Rc<dyn Fn([u8; 32]) -> RemoteMsgHandler> = {
        let strokes = strokes.clone();
        let current_remote = current_remote.clone();
        let canvas_ref = canvas_ref.clone();
        let republish = republish.clone();
        let push_log = push_log.clone();
        Rc::new(move |sender_pk: [u8; 32]| {
            let strokes = strokes.clone();
            let current_remote = current_remote.clone();
            let canvas_ref = canvas_ref.clone();
            let my_pk_for_msg = my_pk;
            let republish_for_msg = republish.clone();
            let push_log = push_log.clone();
            Rc::new(move |s: String| {
                let mut parts = s.split(',');
                let op = parts.next().unwrap_or("");
                // Log replay-class frames so timing issues are visible.
                if op == "S" || op == "f" {
                    push_log(
                        LogLevel::Info,
                        format!("← replay {op} from {}", short_pk(&sender_pk)),
                    );
                }
                match op {
                    "b" => {
                        // b,<color>,<width>,<x>,<y>  — pen begin
                        let color = parts.next().unwrap_or("ffffff").to_string();
                        let width = parts
                            .next()
                            .and_then(|t| t.parse::<f32>().ok())
                            .unwrap_or(DEFAULT_PEN_WIDTH)
                            .clamp(MIN_BRUSH_WIDTH, MAX_BRUSH_WIDTH);
                        let x = parts.next().and_then(|t| t.parse::<f32>().ok());
                        let y = parts.next().and_then(|t| t.parse::<f32>().ok());
                        if let (Some(x), Some(y)) = (x, y) {
                            current_remote.borrow_mut().insert(
                                sender_pk,
                                Stroke {
                                    owner: sender_pk,
                                    color: sanitize_color(&color),
                                    width,
                                    points: vec![(x, y)],
                                    is_eraser: false,
                                },
                            );
                            redraw_with(&canvas_ref, &strokes, None, &current_remote);
                        }
                    }
                    "B" => {
                        // B,<width>,<x>,<y>  — eraser begin (no color;
                        // renderer uses bg as a visual cue while
                        // dragging, then mouseup hit-tests instead of
                        // committing the stroke).
                        let width = parts
                            .next()
                            .and_then(|t| t.parse::<f32>().ok())
                            .unwrap_or(DEFAULT_PEN_WIDTH)
                            .clamp(MIN_BRUSH_WIDTH, MAX_BRUSH_WIDTH);
                        let x = parts.next().and_then(|t| t.parse::<f32>().ok());
                        let y = parts.next().and_then(|t| t.parse::<f32>().ok());
                        if let (Some(x), Some(y)) = (x, y) {
                            current_remote.borrow_mut().insert(
                                sender_pk,
                                Stroke {
                                    owner: sender_pk,
                                    color: CANVAS_BG.to_string(),
                                    width,
                                    points: vec![(x, y)],
                                    is_eraser: true,
                                },
                            );
                            redraw_with(&canvas_ref, &strokes, None, &current_remote);
                        }
                    }
                    "m" | "M" => {
                        let x = parts.next().and_then(|t| t.parse::<f32>().ok());
                        let y = parts.next().and_then(|t| t.parse::<f32>().ok());
                        if let (Some(x), Some(y)) = (x, y) {
                            if let Some(stroke) = current_remote.borrow_mut().get_mut(&sender_pk) {
                                stroke.points.push((x, y));
                            }
                            redraw_with(&canvas_ref, &strokes, None, &current_remote);
                        }
                    }
                    "e" => {
                        // Pen end: commit if it has enough points to
                        // be a visible polyline (single-click pens
                        // that produced one point are dropped).
                        if let Some(stroke) = current_remote.borrow_mut().remove(&sender_pk) {
                            if !stroke.is_eraser && stroke.points.len() >= 2 {
                                strokes.borrow_mut().push(stroke);
                            }
                        }
                        redraw_with(&canvas_ref, &strokes, None, &current_remote);
                    }
                    "E" => {
                        // Eraser end: take the in-flight eraser
                        // stroke, hit-test against committed strokes
                        // from any owner, drop matches, and republish
                        // if any of the deleted strokes were ours
                        // (so the contract reflects the deletion).
                        let eraser = current_remote.borrow_mut().remove(&sender_pk);
                        if let Some(eraser) = eraser {
                            let removed_self = apply_eraser(&strokes, &eraser, my_pk_for_msg);
                            if removed_self {
                                republish_for_msg();
                            }
                        }
                        redraw_with(&canvas_ref, &strokes, None, &current_remote);
                    }
                    "f" => {
                        // f,<color> — atomic full-canvas fill from sender.
                        let color = parts.next().unwrap_or("ffffff").to_string();
                        strokes.borrow_mut().push(Stroke {
                            owner: sender_pk,
                            color: sanitize_color(&color),
                            width: 0.0,
                            points: vec![],
                            is_eraser: false,
                        });
                        redraw_with(&canvas_ref, &strokes, None, &current_remote);
                    }
                    "S" => {
                        // S,<color>,<width>,<x1>,<y1>,<x2>,<y2>,...
                        // Atomic snapshot of a complete pen stroke,
                        // used to replay history when a fresh peer's
                        // datachannel opens. Receiver pushes it
                        // straight into the committed `strokes` list.
                        let color = parts.next().unwrap_or("ffffff").to_string();
                        let width = parts
                            .next()
                            .and_then(|t| t.parse::<f32>().ok())
                            .unwrap_or(DEFAULT_PEN_WIDTH)
                            .clamp(MIN_BRUSH_WIDTH, MAX_BRUSH_WIDTH);
                        let mut points: Vec<(f32, f32)> = Vec::new();
                        loop {
                            let x = parts.next().and_then(|t| t.parse::<f32>().ok());
                            let y = parts.next().and_then(|t| t.parse::<f32>().ok());
                            match (x, y) {
                                (Some(x), Some(y)) => points.push((x, y)),
                                _ => break,
                            }
                        }
                        if points.len() >= 2 {
                            strokes.borrow_mut().push(Stroke {
                                owner: sender_pk,
                                color: sanitize_color(&color),
                                width,
                                points,
                                is_eraser: false,
                            });
                            redraw_with(&canvas_ref, &strokes, None, &current_remote);
                        }
                    }
                    "c" => {
                        // Per-sender clear: drop only what came from this peer
                        // (both committed strokes and any in-flight one).
                        strokes.borrow_mut().retain(|s| s.owner != sender_pk);
                        current_remote.borrow_mut().remove(&sender_pk);
                        redraw_with(&canvas_ref, &strokes, None, &current_remote);
                    }
                    _ => {}
                }
            })
        })
    };

    // Build a per-peer onopen handler. Two jobs:
    //   1. Light up the green "datachannel open" dot in the peer row.
    //   2. Replay our own committed strokes to the freshly-connected
    //      peer over the channel — `f,<color>` for fills, `S,…` for
    //      pen strokes. This is how late joiners catch up on history
    //      now that strokes no longer travel through the contract.
    //      Each peer pushes only their *own* contributions, so a
    //      late joiner needs to call every author whose work they
    //      want to see.
    let make_on_open: Rc<dyn Fn([u8; 32]) -> OpenHandler> = {
        let push_log = push_log.clone();
        let connected_pks = connected_pks.clone();
        let strokes = strokes.clone();
        let peers = peers.clone();
        let my_pk = my_pk;
        Rc::new(move |sender_pk: [u8; 32]| {
            let push_log = push_log.clone();
            let connected_pks = connected_pks.clone();
            let strokes = strokes.clone();
            let peers = peers.clone();
            Rc::new(move || {
                push_log(
                    LogLevel::Info,
                    format!("✓ datachannel open with {}", short_pk(&sender_pk)),
                );
                let mut s = (*connected_pks).clone();
                s.insert(sender_pk);
                connected_pks.set(s);

                // Delay replay slightly so the *peer's* onmessage
                // handler is guaranteed wired by the time our frames
                // arrive. The dc may transition to Open faster than
                // the callee's `ondatachannel` event delivers the
                // channel + wires its `onmessage` listener — frames
                // sent in that window get dropped silently.
                let peers_for_replay = peers.clone();
                let strokes_for_replay = strokes.clone();
                let push_log_for_replay = push_log.clone();
                let win_opt = web_sys::window();
                let cb = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                    let peer = peers_for_replay.borrow().get(&sender_pk).cloned();
                    let Some(peer) = peer else { return };
                    let history: Vec<Stroke> = strokes_for_replay
                        .borrow()
                        .iter()
                        .filter(|s| s.owner == my_pk)
                        .cloned()
                        .collect();
                    let count = history.len();
                    for stroke in history {
                        let _ = peer.send(&stroke_to_wire(&stroke));
                    }
                    if count > 0 {
                        push_log_for_replay(
                            LogLevel::Info,
                            format!("→ replayed {count} stroke(s) to {}", short_pk(&sender_pk)),
                        );
                    }
                });
                if let Some(win) = win_opt {
                    let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
                        cb.as_ref().unchecked_ref(),
                        500,
                    );
                }
                cb.forget();
            })
        })
    };

    // ----------------------------------------------------------------
    //   Snapshot handler — every contract update lands here.
    // ----------------------------------------------------------------
    let on_snapshot = {
        let signing_key = (*signing_key).clone();
        let peers_seen = peers_seen.clone();
        let seen_signals = seen_signals.clone();
        let peers = peers.clone();
        let connected_pks = connected_pks.clone();
        let push_log = push_log.clone();
        let current_remote = current_remote.clone();
        let pending_offers = pending_offers.clone();
        let pending_ice = pending_ice.clone();
        let pending_offer_view = pending_offer_view.clone();
        Callback::from(move |snaps: Vec<RemoteSnapshot>| {
            let my_pk_local = signing_key.verifying_key().to_bytes();

            // Update presence list
            let mut peers_map = (*peers_seen).clone();
            for snap in &snaps {
                if snap.payload.public_key == my_pk_local {
                    continue;
                }
                peers_map.insert(
                    snap.payload.public_key,
                    PeerView {
                        display_name: snap.payload.display_name.clone(),
                        last_seen_ms: snap.payload.timestamp_ms,
                    },
                );
            }
            peers_seen.set(peers_map);

            // NOTE: we deliberately do NOT read `canvas_strokes` from
            // contract snapshots. Strokes flow only over the
            // datachannel between active call peers — the contract
            // entry's `canvas_strokes` field is published always-empty
            // for wire-format compatibility but carries no drawing
            // data. Anyone subscribed to the contract gets presence +
            // signaling only, never the canvas.


            // Process directed signals (offer/answer/ICE/hangup)
            for snap in snaps {
                if snap.payload.public_key == my_pk_local {
                    continue;
                }
                let sender = snap.payload.public_key;
                let sender_short = short_pk(&sender);
                for sig in snap.payload.outbox {
                    if sig.target != my_pk_local {
                        continue;
                    }
                    let already = seen_signals.borrow().get(&sender).copied().unwrap_or(0);
                    if sig.signal_id <= already {
                        continue;
                    }
                    seen_signals.borrow_mut().insert(sender, sig.signal_id);

                    match sig.kind {
                        SignalKind::Offer { sdp } => {
                            // Don't auto-accept. Park the SDP and let
                            // the user explicitly Accept (so a remote
                            // can't open a media-grade pipe to us
                            // without our consent). Skip if we're
                            // already in or building a call to this
                            // peer — caller-side glare.
                            if peers.borrow().contains_key(&sender) {
                                push_log(
                                    LogLevel::Info,
                                    format!("ignoring duplicate offer from {sender_short}"),
                                );
                                continue;
                            }
                            push_log(
                                LogLevel::Info,
                                format!("📞 incoming call from {sender_short}"),
                            );
                            pending_offers.borrow_mut().insert(sender, sdp);
                            let mut v = (*pending_offer_view).clone();
                            v.insert(sender, ());
                            pending_offer_view.set(v);
                        }
                        SignalKind::Answer { sdp } => {
                            push_log(LogLevel::Info, format!("← answer from {sender_short}"));
                            let peers = peers.clone();
                            let push_log = push_log.clone();
                            let sender_short = sender_short.clone();
                            spawn_local(async move {
                                let peer_rc = {
                                    let guard = peers.borrow();
                                    guard.get(&sender).cloned()
                                };
                                let Some(p) = peer_rc else {
                                    push_log(
                                        LogLevel::Warn,
                                        format!("answer from {sender_short} but no active peer"),
                                    );
                                    return;
                                };
                                if let Err(e) = p.accept_answer(&sdp).await {
                                    push_log(LogLevel::Err, format!("accept_answer: {e:?}"));
                                }
                            });
                        }
                        SignalKind::IceCandidate {
                            candidate,
                            sdp_mid,
                            sdp_m_line_index,
                        } => {
                            let c = IceCandidate {
                                candidate,
                                sdp_mid,
                                sdp_m_line_index,
                            };
                            // If a Peer exists, push immediately. If
                            // we're holding a pending offer for this
                            // sender, buffer the candidate so it can
                            // be replayed once the user Accepts. If
                            // neither, drop on the floor (stray ICE
                            // from a peer we never offered/answered
                            // shouldn't open one-sided sessions).
                            if let Some(p) = peers.borrow().get(&sender).cloned() {
                                let push_log = push_log.clone();
                                spawn_local(async move {
                                    if let Err(e) = p.add_remote_ice(c).await {
                                        push_log(
                                            LogLevel::Warn,
                                            format!("addIce: {e:?}"),
                                        );
                                    }
                                });
                            } else if pending_offers.borrow().contains_key(&sender) {
                                pending_ice.borrow_mut().entry(sender).or_default().push(c);
                            }
                        }
                        SignalKind::Hangup => {
                            push_log(LogLevel::Info, format!("← hangup from {sender_short}"));
                            if let Some(p) = peers.borrow_mut().remove(&sender) {
                                p.close();
                            }
                            current_remote.borrow_mut().remove(&sender);
                            let mut c = (*connected_pks).clone();
                            c.remove(&sender);
                            connected_pks.set(c);
                            // Also drop any pending invitation from
                            // this peer — they tore it down before
                            // we got around to accepting.
                            if pending_offers.borrow_mut().remove(&sender).is_some() {
                                let mut v = (*pending_offer_view).clone();
                                v.remove(&sender);
                                pending_offer_view.set(v);
                            }
                            pending_ice.borrow_mut().remove(&sender);
                        }
                    }
                }
            }
        })
    };

    // Tracks whether we've already pushed our initial presence after
    // the WS reaches Subscribed. We can't publish from on_connect
    // synchronously — the socket isn't open yet, send() fails with
    // `WebSocket is not open` and the entry never lands in the
    // contract. Republishing on the first Subscribed transition
    // (via on_status below) ensures the WS is ready.
    let published_once: Rc<RefCell<bool>> = use_mut_ref(|| false);

    let on_status = {
        let status = status.clone();
        let push_log = push_log.clone();
        let published_once = published_once.clone();
        let republish = republish.clone();
        Callback::from(move |s: SignalingStatus| {
            push_log(LogLevel::Info, format!("status: {s:?}"));
            if matches!(s, SignalingStatus::Subscribed) && !*published_once.borrow() {
                *published_once.borrow_mut() = true;
                republish();
            }
            status.set(Some(s));
        })
    };

    // ----------------------------------------------------------------
    //   UI handlers
    // ----------------------------------------------------------------

    // Shared connect implementation — invoked both by the manual
    // Connect button (when defaults need overriding) and by the
    // mount-time auto-connect effect.
    let do_connect: Rc<dyn Fn()> = {
        let signaling = signaling.clone();
        let ws_url = ws_url.clone();
        let instance_id = instance_id.clone();
        let code_hash = code_hash.clone();
        let on_snapshot = on_snapshot.clone();
        let on_status = on_status.clone();
        let push_log = push_log.clone();
        Rc::new(move || {
            // Don't double-start if already connected.
            if signaling.borrow().is_some() {
                return;
            }
            write_storage(STORAGE_KEY_WS, &ws_url);
            write_storage(STORAGE_KEY_INSTANCE, &instance_id);
            write_storage(STORAGE_KEY_CODE_HASH, &code_hash);
            match SignalingClient::start(
                &ws_url,
                &instance_id,
                &code_hash,
                on_snapshot.clone(),
                on_status.clone(),
            ) {
                Ok(c) => {
                    *signaling.borrow_mut() = Some(c);
                    push_log(LogLevel::Info, "signaling client started".into());
                }
                Err(e) => push_log(LogLevel::Err, format!("connect: {e}")),
            }
        })
    };

    let on_connect = {
        let do_connect = do_connect.clone();
        Callback::from(move |_: MouseEvent| do_connect())
    };

    // Auto-connect on first render. Frees the user from clicking
    // Connect every time they open the page — defaults already point
    // at the live contract on the serving node. If the user wants a
    // different node/instance, they can edit the form, refresh, and
    // the saved-storage values become defaults next time.
    {
        let do_connect = do_connect.clone();
        use_effect_with((), move |_| {
            do_connect();
            || ()
        });
    }

    let on_call = {
        let peers = peers.clone();
        let send_signal = send_signal.clone();
        let push_log = push_log.clone();
        let make_remote_msg = make_remote_msg.clone();
        let make_on_open = make_on_open.clone();
        Callback::from(move |target: [u8; 32]| {
            let peers = peers.clone();
            let send_signal = send_signal.clone();
            let push_log = push_log.clone();
            let on_remote_msg = make_remote_msg(target);
            let on_open = make_on_open(target);
            spawn_local(async move {
                if peers.borrow().contains_key(&target) {
                    push_log(
                        LogLevel::Info,
                        format!("already connected to {}", short_pk(&target)),
                    );
                    return;
                }
                let send_signal_for_ice = send_signal.clone();
                let on_local_ice = move |c: Option<IceCandidate>| {
                    let kind = match c {
                        Some(c) => SignalKind::IceCandidate {
                            candidate: c.candidate,
                            sdp_mid: c.sdp_mid,
                            sdp_m_line_index: c.sdp_m_line_index,
                        },
                        None => SignalKind::IceCandidate {
                            candidate: String::new(),
                            sdp_mid: None,
                            sdp_m_line_index: None,
                        },
                    };
                    send_signal_for_ice(target, kind);
                };
                let peer = match Peer::new_caller(on_remote_msg, on_local_ice, on_open) {
                    Ok(p) => p,
                    Err(e) => {
                        push_log(LogLevel::Err, format!("Peer::new_caller: {e:?}"));
                        return;
                    }
                };
                let sdp = match peer.create_offer().await {
                    Ok(s) => s,
                    Err(e) => {
                        push_log(LogLevel::Err, format!("create_offer: {e:?}"));
                        return;
                    }
                };
                push_log(LogLevel::Info, format!("→ offer to {}", short_pk(&target)));
                peers.borrow_mut().insert(target, Rc::new(peer));
                send_signal(target, SignalKind::Offer { sdp });
            });
        })
    };

    // Accept a pending incoming offer. Builds a callee Peer, replays
    // any ICE that arrived before the click, sends Answer back.
    let on_accept_call = {
        let peers = peers.clone();
        let send_signal = send_signal.clone();
        let push_log = push_log.clone();
        let make_remote_msg = make_remote_msg.clone();
        let make_on_open = make_on_open.clone();
        let pending_offers = pending_offers.clone();
        let pending_ice = pending_ice.clone();
        let pending_offer_view = pending_offer_view.clone();
        Callback::from(move |sender: [u8; 32]| {
            let Some(sdp) = pending_offers.borrow_mut().remove(&sender) else {
                return;
            };
            let mut v = (*pending_offer_view).clone();
            v.remove(&sender);
            pending_offer_view.set(v);

            let buffered_ice: Vec<IceCandidate> = pending_ice
                .borrow_mut()
                .remove(&sender)
                .unwrap_or_default();

            let peers = peers.clone();
            let send_signal = send_signal.clone();
            let push_log = push_log.clone();
            let on_remote_msg = make_remote_msg(sender);
            let on_open = make_on_open(sender);
            spawn_local(async move {
                if peers.borrow().contains_key(&sender) {
                    push_log(
                        LogLevel::Info,
                        format!("already connected to {}", short_pk(&sender)),
                    );
                    return;
                }
                let send_signal_for_ice = send_signal.clone();
                let on_local_ice = move |c: Option<IceCandidate>| {
                    let kind = match c {
                        Some(c) => SignalKind::IceCandidate {
                            candidate: c.candidate,
                            sdp_mid: c.sdp_mid,
                            sdp_m_line_index: c.sdp_m_line_index,
                        },
                        None => SignalKind::IceCandidate {
                            candidate: String::new(),
                            sdp_mid: None,
                            sdp_m_line_index: None,
                        },
                    };
                    send_signal_for_ice(sender, kind);
                };
                let peer = match Peer::new_callee(on_remote_msg, on_local_ice, on_open) {
                    Ok(p) => p,
                    Err(e) => {
                        push_log(LogLevel::Err, format!("Peer::new_callee: {e:?}"));
                        return;
                    }
                };
                let answer = match peer.accept_offer(&sdp).await {
                    Ok(s) => s,
                    Err(e) => {
                        push_log(LogLevel::Err, format!("accept_offer: {e:?}"));
                        return;
                    }
                };
                push_log(LogLevel::Info, format!("→ answer to {}", short_pk(&sender)));
                let peer_rc = Rc::new(peer);
                peers.borrow_mut().insert(sender, peer_rc.clone());
                send_signal(sender, SignalKind::Answer { sdp: answer });
                // Drain ICE that arrived before user clicked Accept.
                for c in buffered_ice {
                    if let Err(e) = peer_rc.add_remote_ice(c).await {
                        push_log(LogLevel::Warn, format!("buffered addIce: {e:?}"));
                    }
                }
            });
        })
    };

    let on_reject_call = {
        let pending_offers = pending_offers.clone();
        let pending_ice = pending_ice.clone();
        let pending_offer_view = pending_offer_view.clone();
        let send_signal = send_signal.clone();
        let push_log = push_log.clone();
        Callback::from(move |sender: [u8; 32]| {
            pending_offers.borrow_mut().remove(&sender);
            pending_ice.borrow_mut().remove(&sender);
            let mut v = (*pending_offer_view).clone();
            v.remove(&sender);
            pending_offer_view.set(v);
            // Politely tell the caller we're not picking up — they'll
            // tear down their offer-side PeerConnection without
            // waiting for ICE timeout.
            send_signal(sender, SignalKind::Hangup);
            push_log(
                LogLevel::Info,
                format!("rejected call from {}", short_pk(&sender)),
            );
        })
    };

    let on_disconnect = {
        let peers = peers.clone();
        let send_signal = send_signal.clone();
        let push_log = push_log.clone();
        let connected_pks = connected_pks.clone();
        let current_remote = current_remote.clone();
        Callback::from(move |target: [u8; 32]| {
            if let Some(p) = peers.borrow_mut().remove(&target) {
                p.close();
            }
            current_remote.borrow_mut().remove(&target);
            let mut c = (*connected_pks).clone();
            c.remove(&target);
            connected_pks.set(c);
            send_signal(target, SignalKind::Hangup);
            push_log(
                LogLevel::Info,
                format!("disconnected from {}", short_pk(&target)),
            );
        })
    };

    let on_hangup_all = {
        let peers = peers.clone();
        let send_signal = send_signal.clone();
        let push_log = push_log.clone();
        let connected_pks = connected_pks.clone();
        let current_remote = current_remote.clone();
        Callback::from(move |_: MouseEvent| {
            let targets: Vec<[u8; 32]> = peers.borrow().keys().copied().collect();
            for t in &targets {
                if let Some(p) = peers.borrow_mut().remove(t) {
                    p.close();
                }
                send_signal(*t, SignalKind::Hangup);
            }
            current_remote.borrow_mut().clear();
            connected_pks.set(HashSet::new());
            if !targets.is_empty() {
                push_log(
                    LogLevel::Info,
                    format!("hung up on {} peer(s)", targets.len()),
                );
            }
        })
    };

    // Per-sender clear: wipe *my* committed + in-flight strokes only,
    // then broadcast `c` so other peers wipe their copy of mine too.
    // Republish strips strokes from contract.
    let on_clear_mine = {
        let strokes = strokes.clone();
        let current_local = current_local.clone();
        let canvas_ref = canvas_ref.clone();
        let current_remote = current_remote.clone();
        let peers = peers.clone();
        let republish = republish.clone();
        let my_pk = my_pk;
        Callback::from(move |_: MouseEvent| {
            strokes.borrow_mut().retain(|s| s.owner != my_pk);
            current_local.borrow_mut().take();
            redraw_with(&canvas_ref, &strokes, Some(&current_local), &current_remote);
            for p in peers.borrow().values() {
                let _ = p.send("c");
            }
            republish();
        })
    };

    // -- Pen handlers --------------------------------------------------
    //
    // The mousedown / move / up / leave triplet runs on the canvas:
    //   down → start a new stroke and broadcast "b,color,x,y"
    //   move → push a point + broadcast "m,x,y" (only while drawing)
    //   up/leave → finalize stroke + broadcast "e" + republish to contract
    //
    // We send *every* move event without throttling. WebRTC datachannel
    // handles hundreds of small text frames per second easily, and the
    // alternative (throttle to e.g. 60Hz with a timer) introduces lag
    // visible to the human drawing the line. The contract republish
    // only fires on stroke-end so it doesn't get hammered.

    let on_mousedown = {
        let canvas_ref = canvas_ref.clone();
        let strokes = strokes.clone();
        let current_local = current_local.clone();
        let current_remote = current_remote.clone();
        let is_drawing = is_drawing.clone();
        let pen_color = pen_color.clone();
        let pen_width = pen_width.clone();
        let current_tool = current_tool.clone();
        let peers = peers.clone();
        let republish = republish.clone();
        let my_pk = my_pk;
        Callback::from(move |e: MouseEvent| {
            let Some((x, y)) = canvas_xy(&canvas_ref, &e) else { return };
            let tool = *current_tool;
            let (color, width) = stroke_params(tool, &pen_color, *pen_width);
            match tool {
                Tool::Fill => {
                    let stroke = Stroke {
                        owner: my_pk,
                        color: color.clone(),
                        width: 0.0,
                        points: vec![],
                        is_eraser: false,
                    };
                    strokes.borrow_mut().push(stroke);
                    redraw_with(&canvas_ref, &strokes, Some(&current_local), &current_remote);
                    let hex = color.trim_start_matches('#');
                    let frame = format!("f,{hex}");
                    for p in peers.borrow().values() {
                        let _ = p.send(&frame);
                    }
                    republish();
                }
                Tool::Pen => {
                    *is_drawing.borrow_mut() = true;
                    *current_local.borrow_mut() = Some(Stroke {
                        owner: my_pk,
                        color: color.clone(),
                        width,
                        points: vec![(x, y)],
                        is_eraser: false,
                    });
                    redraw_with(&canvas_ref, &strokes, Some(&current_local), &current_remote);
                    let hex = color.trim_start_matches('#');
                    let frame = format!("b,{hex},{width:.1},{x:.4},{y:.4}");
                    for p in peers.borrow().values() {
                        let _ = p.send(&frame);
                    }
                }
                Tool::Eraser => {
                    *is_drawing.borrow_mut() = true;
                    *current_local.borrow_mut() = Some(Stroke {
                        owner: my_pk,
                        color: CANVAS_BG.to_string(),
                        width,
                        points: vec![(x, y)],
                        is_eraser: true,
                    });
                    redraw_with(&canvas_ref, &strokes, Some(&current_local), &current_remote);
                    let frame = format!("B,{width:.1},{x:.4},{y:.4}");
                    for p in peers.borrow().values() {
                        let _ = p.send(&frame);
                    }
                }
            }
        })
    };

    let on_mousemove = {
        let canvas_ref = canvas_ref.clone();
        let strokes = strokes.clone();
        let current_local = current_local.clone();
        let current_remote = current_remote.clone();
        let is_drawing = is_drawing.clone();
        let peers = peers.clone();
        Callback::from(move |e: MouseEvent| {
            if !*is_drawing.borrow() {
                return;
            }
            let Some((x, y)) = canvas_xy(&canvas_ref, &e) else { return };
            let is_eraser = current_local
                .borrow()
                .as_ref()
                .map(|s| s.is_eraser)
                .unwrap_or(false);
            if let Some(stroke) = current_local.borrow_mut().as_mut() {
                stroke.points.push((x, y));
            }
            redraw_with(&canvas_ref, &strokes, Some(&current_local), &current_remote);
            // Uppercase opcode for eraser so the receiver knows to
            // hit-test on `E` instead of committing the stroke.
            let frame = if is_eraser {
                format!("M,{x:.4},{y:.4}")
            } else {
                format!("m,{x:.4},{y:.4}")
            };
            for p in peers.borrow().values() {
                let _ = p.send(&frame);
            }
        })
    };

    let end_stroke: Rc<dyn Fn()> = {
        let strokes = strokes.clone();
        let current_local = current_local.clone();
        let current_remote = current_remote.clone();
        let is_drawing = is_drawing.clone();
        let peers = peers.clone();
        let canvas_ref = canvas_ref.clone();
        let republish = republish.clone();
        let my_pk = my_pk;
        Rc::new(move || {
            if !*is_drawing.borrow() {
                return;
            }
            *is_drawing.borrow_mut() = false;
            let taken = current_local.borrow_mut().take();
            let mut should_republish = false;
            let mut end_op = "e";
            if let Some(stroke) = taken {
                if stroke.is_eraser {
                    end_op = "E";
                    // Run hit-test against committed strokes from
                    // any owner, drop matches. If we removed any of
                    // our own strokes, republish so the contract
                    // shrinks our canvas_strokes too — peers not in
                    // the call learn about it via the next snapshot.
                    if apply_eraser(&strokes, &stroke, my_pk) {
                        should_republish = true;
                    }
                } else if stroke.points.len() >= 2 {
                    strokes.borrow_mut().push(stroke);
                    should_republish = true;
                }
            }
            redraw_with(&canvas_ref, &strokes, Some(&current_local), &current_remote);
            for p in peers.borrow().values() {
                let _ = p.send(end_op);
            }
            if should_republish {
                republish();
            }
        })
    };

    let on_mouseup = {
        let end_stroke = end_stroke.clone();
        Callback::from(move |_: MouseEvent| end_stroke())
    };
    let on_mouseleave = {
        let end_stroke = end_stroke.clone();
        Callback::from(move |_: MouseEvent| end_stroke())
    };

    // Set the browser tab title via the outer shell. Without this
    // postMessage the tab just says "Freenet" — the shell's HTML
    // title — even when the actual app inside has a name.
    use_effect_with((), |_| {
        set_shell_title("Freenet Collab Draw");
        || ()
    });

    // Heartbeat: republish our own contract entry every 15s. This keeps
    // our `timestamp_ms` fresh on the contract so other peers can tell
    // we're still around (their UI marks rows offline once
    // `now - timestamp_ms > ONLINE_WINDOW_MS`). The first publish on
    // Subscribed (in on_status) primes the entry; the heartbeat keeps
    // it warm without requiring user action.
    {
        let republish = republish.clone();
        use_effect_with((), move |_| {
            let cb = Closure::<dyn FnMut()>::new(move || republish());
            let mut handle: i32 = -1;
            if let Some(window) = web_sys::window() {
                handle = window
                    .set_interval_with_callback_and_timeout_and_arguments_0(
                        cb.as_ref().unchecked_ref(),
                        HEARTBEAT_MS as i32,
                    )
                    .unwrap_or(-1);
            }
            // Move the closure into the cleanup so it survives across
            // every tick — dropping it would null out the JS callback.
            let cb_keep = cb;
            move || {
                if let Some(window) = web_sys::window() {
                    if handle >= 0 {
                        window.clear_interval_with_handle(handle);
                    }
                }
                drop(cb_keep);
            }
        });
    }

    // Online-indicator refresh tick: bump a counter every 5s so the
    // peer-row "online" pill recomputes against `Date.now()` without
    // waiting for a fresh contract snapshot.
    {
        let now_tick = now_tick.clone();
        use_effect_with((), move |_| {
            let now_tick_for_cb = now_tick.clone();
            let cb = Closure::<dyn FnMut()>::new(move || {
                now_tick_for_cb.set(*now_tick_for_cb + 1);
            });
            let mut handle: i32 = -1;
            if let Some(window) = web_sys::window() {
                handle = window
                    .set_interval_with_callback_and_timeout_and_arguments_0(
                        cb.as_ref().unchecked_ref(),
                        ONLINE_TICK_MS as i32,
                    )
                    .unwrap_or(-1);
            }
            let cb_keep = cb;
            move || {
                if let Some(window) = web_sys::window() {
                    if handle >= 0 {
                        window.clear_interval_with_handle(handle);
                    }
                }
                drop(cb_keep);
            }
        });
    }

    // Resize canvas backing store to its CSS pixel size on mount + on
    // window resize, so drawings aren't stretched.
    {
        let canvas_ref = canvas_ref.clone();
        let strokes = strokes.clone();
        let current_local = current_local.clone();
        let current_remote = current_remote.clone();
        use_effect_with((), move |_| {
            let resize = {
                let canvas_ref = canvas_ref.clone();
                let strokes = strokes.clone();
                let current_local = current_local.clone();
                let current_remote = current_remote.clone();
                Closure::<dyn FnMut()>::new(move || {
                    if let Some(c) = canvas_ref.cast::<HtmlCanvasElement>() {
                        let rect = c.get_bounding_client_rect();
                        c.set_width(rect.width() as u32);
                        c.set_height(rect.height() as u32);
                        redraw_with(&canvas_ref, &strokes, Some(&current_local), &current_remote);
                    }
                })
            };
            if let Some(window) = web_sys::window() {
                let _ = window.add_event_listener_with_callback(
                    "resize",
                    resize.as_ref().unchecked_ref(),
                );
                resize.as_ref().unchecked_ref::<js_sys::Function>().call0(&wasm_bindgen::JsValue::NULL).ok();
                resize.forget();
            }
            || ()
        });
    }

    // ----------------------------------------------------------------
    //   Render
    // ----------------------------------------------------------------

    let connected = matches!(*status, Some(SignalingStatus::Subscribed));
    let any_calls = !connected_pks.is_empty();
    // Touch now_tick so Yew tracks it as a render dependency — drives
    // the periodic refresh of the online indicator.
    let _ = *now_tick;
    let now_ms = js_sys::Date::now() as u64;

    let on_color_change = {
        let pen_color = pen_color.clone();
        Callback::from(move |e: InputEvent| {
            let v = input_value(e);
            write_storage(STORAGE_KEY_COLOR, &v);
            pen_color.set(v);
        })
    };

    let on_pick_swatch = {
        let pen_color = pen_color.clone();
        Callback::from(move |hex: String| {
            write_storage(STORAGE_KEY_COLOR, &hex);
            pen_color.set(hex);
        })
    };

    let on_pick_tool = {
        let current_tool = current_tool.clone();
        Callback::from(move |t: Tool| current_tool.set(t))
    };

    let on_width_change = {
        let pen_width = pen_width.clone();
        Callback::from(move |e: InputEvent| {
            let raw = input_value(e);
            if let Ok(v) = raw.parse::<f32>() {
                let clamped = v.clamp(MIN_BRUSH_WIDTH, MAX_BRUSH_WIDTH);
                write_storage(STORAGE_KEY_WIDTH, &clamped.to_string());
                pen_width.set(clamped);
            }
        })
    };

    html! {
        <>
        <header class="bar">
            <h1>{"Freenet Collab Draw"}</h1>
            <span class="pill">{"me: "}{ short_str(&my_pk_hex) }</span>
            { render_status_pill(&status) }
            if !connected {
                <input
                    class="url"
                    placeholder="ws://127.0.0.1:7509"
                    value={(*ws_url).clone()}
                    oninput={ on_input(&ws_url) }
                />
                <input
                    class="text"
                    placeholder="contract instance id (base58)"
                    value={(*instance_id).clone()}
                    oninput={ on_input(&instance_id) }
                />
                <input
                    class="text"
                    placeholder="contract code hash (base58)"
                    value={(*code_hash).clone()}
                    oninput={ on_input(&code_hash) }
                />
                <button class="primary" onclick={on_connect}>{"Connect"}</button>
            }
            <input
                class="text"
                placeholder="display name"
                value={(*display_name).clone()}
                oninput={
                    let n = display_name.clone();
                    let cell = display_name_cell.clone();
                    let republish = republish.clone();
                    let sk_for_hash = (*signing_key).clone();
                    Callback::from(move |e: InputEvent| {
                        let v = input_value(e);
                        write_storage(STORAGE_KEY_NAME, &v);
                        // Persist into the URL hash so a sandbox
                        // reload restores name+identity together.
                        save_identity_to_hash(&sk_for_hash, &v);
                        // Update the heartbeat-visible cell *before*
                        // triggering republish so it ships with the
                        // new name on this very call, not the next
                        // heartbeat 15s later.
                        *cell.borrow_mut() = v.clone();
                        n.set(v);
                        republish();
                    })
                }
            />
            <div class="tools">
                { render_tool_btn(Tool::Pen,    "✏",  "Pen",    *current_tool, &on_pick_tool) }
                { render_tool_btn(Tool::Eraser, "⌫",  "Eraser", *current_tool, &on_pick_tool) }
                { render_tool_btn(Tool::Fill,   "🪣", "Fill",   *current_tool, &on_pick_tool) }
            </div>
            <input
                class="color"
                type="color"
                value={(*pen_color).clone()}
                oninput={on_color_change}
                title="pen color (custom)"
                disabled={*current_tool == Tool::Eraser}
            />
            <div class="palette" title="quick colors">
                { for COLOR_PALETTE.iter().map(|hex| {
                    let hex_owned = (*hex).to_string();
                    let active = *current_tool != Tool::Eraser && pen_color.eq_ignore_ascii_case(hex);
                    let on_pick = on_pick_swatch.clone();
                    let onclick = Callback::from(move |_: MouseEvent| on_pick.emit(hex_owned.clone()));
                    let cls = if active { "swatch active" } else { "swatch" };
                    let style = format!("background:{hex}");
                    html! { <button class={cls} {style} {onclick} title={hex.to_string()} disabled={*current_tool == Tool::Eraser}></button> }
                }) }
            </div>
            if *current_tool != Tool::Fill {
                <label class="width-ctrl" title="brush size">
                    <input
                        type="range"
                        min={MIN_BRUSH_WIDTH.to_string()}
                        max={MAX_BRUSH_WIDTH.to_string()}
                        step="1"
                        value={pen_width.to_string()}
                        oninput={on_width_change}
                    />
                    <span class="width-num">{format!("{}px", *pen_width as i32)}</span>
                </label>
            }
            <button onclick={on_clear_mine} title="clear my strokes (broadcasts to peers)">{"Clear mine"}</button>
            if any_calls {
                <button class="danger" onclick={on_hangup_all}>{"Hang up all"}</button>
            }
        </header>

        <main>
            <aside class="peers">
                <h2>{"Peers in room"}</h2>
                if peers_seen.is_empty() {
                    <div class="peer-id">{"no peers yet — share the contract id with a friend, both connect"}</div>
                }
                {
                    {
                        let mut shown: Vec<(&[u8;32], &PeerView, bool, bool)> = peers_seen
                            .iter()
                            .filter_map(|(pk, view)| {
                                let is_connected = connected_pks.contains(pk);
                                let has_pending = pending_offer_view.contains_key(pk);
                                let is_online = now_ms.saturating_sub(view.last_seen_ms) < ONLINE_WINDOW_MS;
                                // Hide peers we haven't heard from in
                                // ONLINE_WINDOW_MS. Exception: keep
                                // showing anyone in an active call or
                                // with a pending invitation — their
                                // datachannel may be healthy even if
                                // their contract heartbeat lapsed.
                                (is_online || is_connected || has_pending).then_some(
                                    (pk, view, is_connected, is_online)
                                )
                            })
                            .collect();
                        shown.sort_by(|a, b| b.1.last_seen_ms.cmp(&a.1.last_seen_ms));
                        if shown.is_empty() && !peers_seen.is_empty() {
                            html! { <div class="peer-id muted">{"all peers offline · waiting for activity"}</div> }
                        } else {
                            html! {
                                { for shown.into_iter().map(|(pk, view, is_connected, is_online)| {
                                    render_peer_row(
                                        pk,
                                        view,
                                        &on_call,
                                        &on_disconnect,
                                        is_connected,
                                        is_online,
                                    )
                                }) }
                            }
                        }
                    }
                }
                <footer class="sidebar-footer">
                    <a
                        href={REPO_URL}
                        target="_blank"
                        rel="noopener noreferrer"
                        title="Source on GitHub"
                    >
                        {"github.com/Basedfloppa/freenet-webrtc-poc"}
                    </a>
                </footer>
            </aside>

            <section class="stage">
                if !pending_offer_view.is_empty() {
                    <div class="invitations">
                        { for pending_offer_view.keys().map(|pk| {
                            let display = peers_seen
                                .get(pk)
                                .map(|v| v.display_name.clone())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "(no name)".into());
                            render_invitation(pk, &display, &on_accept_call, &on_reject_call)
                        }) }
                    </div>
                }
                <div class="canvas-wrap">
                    <canvas
                        ref={canvas_ref}
                        class="draw-canvas"
                        onmousedown={on_mousedown}
                        onmousemove={on_mousemove}
                        onmouseup={on_mouseup}
                        onmouseleave={on_mouseleave}
                    ></canvas>
                    <div class="canvas-hint">
                        {
                            if any_calls {
                                format!("connected to {} peer(s) · drag to draw", connected_pks.len())
                            } else if connected {
                                "subscribed · click Call on a peer to start drawing together".into()
                            } else {
                                "not connected · paste contract id and Connect".into()
                            }
                        }
                    </div>
                </div>
                <div class="log">
                    { for log.iter().rev().take(40).map(|l| html! {
                        <div class={ classes!("line", match l.level {
                            LogLevel::Info => "",
                            LogLevel::Warn => "warn",
                            LogLevel::Err => "err",
                        }) }>{ &l.text }</div>
                    }) }
                </div>
            </section>
        </main>
        </>
    }
}

// -- Canvas drawing ---------------------------------------------------

/// Wrapper around `redraw` that lets the caller pass `None` for
/// `current_local` (used in remote-message handlers where we don't
/// have access to the local pen state — and we don't need it because
/// the same `mousemove` already redrew it).
fn redraw_with(
    canvas_ref: &NodeRef,
    strokes: &Rc<RefCell<Vec<Stroke>>>,
    current_local: Option<&Rc<RefCell<Option<Stroke>>>>,
    current_remote: &Rc<RefCell<HashMap<[u8; 32], Stroke>>>,
) {
    let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() else { return };
    let Ok(Some(ctx)) = canvas.get_context("2d") else { return };
    let Ok(ctx) = ctx.dyn_into::<CanvasRenderingContext2d>() else { return };

    let w = canvas.width() as f64;
    let h = canvas.height() as f64;
    ctx.set_fill_style_str(CANVAS_BG);
    ctx.fill_rect(0.0, 0.0, w, h);

    // Subtle grid for visual reference. Drawn before strokes so a
    // fill stroke wipes it (matches user expectation that fill
    // covers everything).
    ctx.set_stroke_style_str("#141c25");
    ctx.set_line_width(1.0);
    let step = 40.0;
    let mut x = 0.0;
    while x < w {
        ctx.begin_path();
        ctx.move_to(x, 0.0);
        ctx.line_to(x, h);
        ctx.stroke();
        x += step;
    }
    let mut y = 0.0;
    while y < h {
        ctx.begin_path();
        ctx.move_to(0.0, y);
        ctx.line_to(w, y);
        ctx.stroke();
        y += step;
    }

    ctx.set_line_cap("round");
    ctx.set_line_join("round");

    for s in strokes.borrow().iter() {
        draw_stroke(&ctx, s, w, h);
    }
    if let Some(cl) = current_local {
        if let Some(s) = cl.borrow().as_ref() {
            draw_stroke(&ctx, s, w, h);
        }
    }
    for s in current_remote.borrow().values() {
        draw_stroke(&ctx, s, w, h);
    }
}

fn draw_stroke(ctx: &CanvasRenderingContext2d, s: &Stroke, w: f64, h: f64) {
    // Empty points = fill marker.
    if s.points.is_empty() {
        ctx.set_fill_style_str(&s.color);
        ctx.fill_rect(0.0, 0.0, w, h);
        return;
    }
    let line_width = (s.width as f64).max(1.0);
    ctx.set_line_width(line_width);
    if s.points.len() == 1 {
        // Single click = filled disc, sized like a brush dot.
        let (px, py) = s.points[0];
        ctx.set_fill_style_str(&s.color);
        ctx.begin_path();
        let _ = ctx.arc(
            px as f64 * w,
            py as f64 * h,
            (line_width / 2.0).max(1.0),
            0.0,
            std::f64::consts::TAU,
        );
        ctx.fill();
        return;
    }
    ctx.set_stroke_style_str(&s.color);
    ctx.begin_path();
    let (x0, y0) = s.points[0];
    ctx.move_to(x0 as f64 * w, y0 as f64 * h);
    for &(x, y) in &s.points[1..] {
        ctx.line_to(x as f64 * w, y as f64 * h);
    }
    ctx.stroke();
}

/// Translate a mouse event into canvas-relative fractional coordinates.
/// Returns None if the canvas isn't mounted yet.
fn canvas_xy(canvas_ref: &NodeRef, e: &MouseEvent) -> Option<(f32, f32)> {
    let canvas = canvas_ref.cast::<HtmlCanvasElement>()?;
    let rect = canvas.get_bounding_client_rect();
    let w = rect.width().max(1.0) as f32;
    let h = rect.height().max(1.0) as f32;
    let x = (e.client_x() as f32 - rect.left() as f32) / w;
    let y = (e.client_y() as f32 - rect.top() as f32) / h;
    Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)))
}

// -- Peer-row rendering -----------------------------------------------

fn render_peer_row(
    pk: &[u8; 32],
    view: &PeerView,
    on_call: &Callback<[u8; 32]>,
    on_disconnect: &Callback<[u8; 32]>,
    is_connected: bool,
    is_online: bool,
) -> Html {
    let pk_owned = *pk;
    let onclick_call = {
        let on_call = on_call.clone();
        Callback::from(move |_: MouseEvent| on_call.emit(pk_owned))
    };
    let onclick_disconnect = {
        let on_disconnect = on_disconnect.clone();
        Callback::from(move |_: MouseEvent| on_disconnect.emit(pk_owned))
    };
    let display = if view.display_name.is_empty() {
        "(no name)".to_string()
    } else {
        view.display_name.clone()
    };
    let row_class = match (is_connected, is_online) {
        (true, _) => "peer-row connected",
        (false, true) => "peer-row online",
        (false, false) => "peer-row offline",
    };
    let dot_class = if is_connected {
        "dot ok"
    } else if is_online {
        "dot online"
    } else {
        "dot offline"
    };
    let dot_title = if is_connected {
        "datachannel open"
    } else if is_online {
        "online (recent heartbeat)"
    } else {
        "offline (no recent heartbeat)"
    };
    html! {
        <div class={row_class}>
            <div class="peer-name">
                <span class={dot_class} title={dot_title}></span>
                { display }
            </div>
            <div class="peer-id">{ short_pk(pk) }</div>
            <div class="peer-actions">
                if is_connected {
                    <button class="danger" onclick={onclick_disconnect}>{"Disconnect"}</button>
                } else {
                    <button onclick={onclick_call} disabled={!is_online} title={if is_online { "" } else { "peer offline" }}>{"Call"}</button>
                }
            </div>
        </div>
    }
}

fn render_tool_btn(
    tool: Tool,
    icon: &str,
    label: &str,
    current: Tool,
    on_pick: &Callback<Tool>,
) -> Html {
    let cls = if current == tool { "tool active" } else { "tool" };
    let on_pick = on_pick.clone();
    let onclick = Callback::from(move |_: MouseEvent| on_pick.emit(tool));
    html! {
        <button class={cls} {onclick} title={label.to_string()}>
            <span class="tool-icon">{icon}</span>
            <span class="tool-label">{label}</span>
        </button>
    }
}

/// Serialize a committed stroke for replay over the datachannel when
/// a fresh peer connects. `f,<color>` for fills (atomic, no points);
/// `S,<color>,<width>,<x>,<y>,…` for everything else.
fn stroke_to_wire(s: &Stroke) -> String {
    let hex = s.color.trim_start_matches('#');
    if s.points.is_empty() {
        return format!("f,{hex}");
    }
    let mut frame = format!("S,{hex},{:.1}", s.width);
    for &(x, y) in &s.points {
        frame.push_str(&format!(",{x:.4},{y:.4}"));
    }
    frame
}

/// Hit-test the given eraser stroke against the committed `strokes`
/// list and remove any stroke whose geometry intersects. Returns
/// `true` if at least one stroke owned by `my_pk` was removed — the
/// caller uses this to decide whether to republish the contract entry.
///
/// Strokes from other owners are still removed locally (optimistic);
/// the original owner runs the same hit-test on their side when they
/// receive the eraser frames over their datachannel and republishes
/// the shorter list. Until that owner's snapshot lands, our local
/// view shows the strokes already gone.
fn apply_eraser(
    strokes: &Rc<RefCell<Vec<Stroke>>>,
    eraser: &Stroke,
    my_pk: [u8; 32],
) -> bool {
    if eraser.points.is_empty() {
        return false;
    }
    let radius_frac = (eraser.width / 2.0) / 1000.0 + 0.005;
    let r2 = radius_frac * radius_frac;
    let mut removed_self = false;
    strokes.borrow_mut().retain(|s| {
        let hit = stroke_intersects_eraser(s, eraser, r2);
        if hit && s.owner == my_pk {
            removed_self = true;
        }
        !hit
    });
    removed_self
}

/// True if any point of `target` is within `r2` (squared radius, in
/// fractional canvas units) of any point of `eraser`. A fill stroke
/// (empty `points`) covers the whole canvas so it's always hit by an
/// eraser swipe of any size — punching a hole in a fill removes the
/// fill entirely, which matches user expectation ("the fill goes away
/// where I erased").
fn stroke_intersects_eraser(target: &Stroke, eraser: &Stroke, r2: f32) -> bool {
    if target.points.is_empty() {
        return true;
    }
    for &(ex, ey) in &eraser.points {
        for &(px, py) in &target.points {
            let dx = ex - px;
            let dy = ey - py;
            if dx * dx + dy * dy < r2 {
                return true;
            }
        }
    }
    false
}

/// Resolve the effective stroke color + width for a tool.
/// Eraser overrides color to canvas bg so the strokes overwrite
/// previous ones visually; both pen and eraser respect the user's
/// width slider so they can fine-tune either one.
fn stroke_params(tool: Tool, pen_color: &str, pen_width: f32) -> (String, f32) {
    let color = match tool {
        Tool::Eraser => CANVAS_BG.to_string(),
        Tool::Pen | Tool::Fill => pen_color.to_string(),
    };
    (color, pen_width)
}

fn render_invitation(
    pk: &[u8; 32],
    display: &str,
    on_accept: &Callback<[u8; 32]>,
    on_reject: &Callback<[u8; 32]>,
) -> Html {
    let pk_owned = *pk;
    let onclick_accept = {
        let on_accept = on_accept.clone();
        Callback::from(move |_: MouseEvent| on_accept.emit(pk_owned))
    };
    let onclick_reject = {
        let on_reject = on_reject.clone();
        Callback::from(move |_: MouseEvent| on_reject.emit(pk_owned))
    };
    html! {
        <div class="invitation">
            <span class="ring">{"📞"}</span>
            <div class="invitation-text">
                <div class="invitation-title">{"Incoming call from "}<strong>{display}</strong></div>
                <div class="invitation-id">{ short_pk(pk) }</div>
            </div>
            <button class="primary" onclick={onclick_accept}>{"Accept"}</button>
            <button class="danger" onclick={onclick_reject}>{"Reject"}</button>
        </div>
    }
}

fn render_status_pill(status: &Option<SignalingStatus>) -> Html {
    match status {
        None => html! { <span class="pill">{"not connected"}</span> },
        Some(SignalingStatus::Connecting) => html! { <span class="pill">{"connecting…"}</span> },
        Some(SignalingStatus::Subscribed) => {
            html! { <span class="pill ok">{"subscribed"}</span> }
        }
        Some(SignalingStatus::Error(e)) => {
            html! { <span class="pill err" title={e.clone()}>{"error"}</span> }
        }
    }
}

// -- Helpers ----------------------------------------------------------

/// Default WS URL: derive from the page origin so the webapp talks to
/// the same freenet node that served it. Falls back to localhost if
/// we can't read `window.location` (e.g. trunk-serve standalone).
fn default_ws_url() -> String {
    let win = match web_sys::window() {
        Some(w) => w,
        None => return DEFAULT_WS.into(),
    };
    let host = win.location().hostname().unwrap_or_else(|_| "127.0.0.1".into());
    let port = win
        .location()
        .port()
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "7509".into());
    format!("ws://{host}:{port}")
}

/// Resolve the signing key on startup. URL hash wins over localStorage:
/// inside Freenet's null-origin sandbox iframe, every reload is a fresh
/// opaque origin so `localStorage` is per-load and useless for identity
/// continuity. The hash, by contrast, survives reload (the outer shell
/// preserves it across iframe rebuilds via the `type: 'hash'`
/// postMessage protocol) and can be copy-pasted to a different Freenet
/// node to log in as the same publisher there.
fn load_or_generate_sk() -> (SigningKey, Option<String>) {
    // 1. Hash (persistent inside sandbox iframe, portable across nodes)
    if let Some(parsed) = parse_hash() {
        return parsed;
    }
    // 2. localStorage (works in standalone trunk-serve / non-sandbox)
    if let Some(b58) = read_storage(STORAGE_KEY_SK) {
        if let Ok(bytes) = bs58::decode(b58.trim()).into_vec() {
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                let sk = SigningKey::from_bytes(&arr);
                let name = read_storage(STORAGE_KEY_NAME);
                // Mirror to hash so it persists past the next sandbox
                // reload too — even if localStorage was used to seed.
                save_identity_to_hash(&sk, name.as_deref().unwrap_or(""));
                return (sk, name);
            }
        }
    }
    // 3. Fresh key. Mirror to *both* stores.
    let sk = SigningKey::generate(&mut OsRng);
    write_storage(STORAGE_KEY_SK, &bs58::encode(sk.to_bytes()).into_string());
    save_identity_to_hash(&sk, "");
    (sk, None)
}

/// Try to extract `(SigningKey, name)` from `window.location.hash`.
/// Format: `#k=<base58_32>&n=<urlencoded_name>`. Either field may be
/// missing; only the key is required to return Some.
fn parse_hash() -> Option<(SigningKey, Option<String>)> {
    let win = web_sys::window()?;
    let hash = win.location().hash().ok()?;
    let trimmed = hash.trim_start_matches('#');
    if trimmed.is_empty() {
        return None;
    }
    let mut seed: Option<[u8; 32]> = None;
    let mut name: Option<String> = None;
    for pair in trimmed.split('&') {
        let mut kv = pair.splitn(2, '=');
        let k = kv.next()?;
        let v = kv.next().unwrap_or("");
        match k {
            "k" => {
                if let Ok(bytes) = bs58::decode(v).into_vec() {
                    if bytes.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&bytes);
                        seed = Some(arr);
                    }
                }
            }
            "n" => {
                if let Ok(decoded) = js_sys::decode_uri_component(v) {
                    if let Some(s) = decoded.as_string() {
                        if !s.is_empty() {
                            name = Some(s);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    seed.map(|s| (SigningKey::from_bytes(&s), name))
}

/// Write the current identity into both the iframe's own
/// `location.hash` (so subsequent in-page reads see it) and the outer
/// shell's URL via the `type: 'hash'` postMessage protocol (so a
/// browser refresh — which reloads the *outer* document — preserves
/// it across the iframe rebuild that follows).
fn save_identity_to_hash(sk: &SigningKey, name: &str) {
    let seed_b58 = bs58::encode(sk.to_bytes()).into_string();
    let hash = if name.is_empty() {
        format!("#k={seed_b58}")
    } else {
        let encoded = js_sys::encode_uri_component(name)
            .as_string()
            .unwrap_or_default();
        format!("#k={seed_b58}&n={encoded}")
    };
    let Some(win) = web_sys::window() else { return };
    // Local hash so this page sees it on next read.
    let _ = win.location().set_hash(&hash);
    // Outer shell so a future reload starts the iframe with the hash.
    if let Some(parent) = win.parent().ok().flatten() {
        let msg = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &msg,
            &wasm_bindgen::JsValue::from_str("__freenet_shell__"),
            &wasm_bindgen::JsValue::TRUE,
        );
        let _ = js_sys::Reflect::set(
            &msg,
            &wasm_bindgen::JsValue::from_str("type"),
            &wasm_bindgen::JsValue::from_str("hash"),
        );
        let _ = js_sys::Reflect::set(
            &msg,
            &wasm_bindgen::JsValue::from_str("hash"),
            &wasm_bindgen::JsValue::from_str(&hash),
        );
        let _ = parent.post_message(&msg, "*");
    }
}

/// Push a tab title to the Freenet outer shell. Without this the
/// browser tab just says "Freenet" — the shell's HTML title — even
/// when the actual app inside the sandbox iframe has its own identity.
fn set_shell_title(title: &str) {
    let Some(win) = web_sys::window() else { return };
    let Some(parent) = win.parent().ok().flatten() else { return };
    let msg = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &msg,
        &wasm_bindgen::JsValue::from_str("__freenet_shell__"),
        &wasm_bindgen::JsValue::TRUE,
    );
    let _ = js_sys::Reflect::set(
        &msg,
        &wasm_bindgen::JsValue::from_str("type"),
        &wasm_bindgen::JsValue::from_str("title"),
    );
    let _ = js_sys::Reflect::set(
        &msg,
        &wasm_bindgen::JsValue::from_str("title"),
        &wasm_bindgen::JsValue::from_str(title),
    );
    let _ = parent.post_message(&msg, "*");
}

fn read_storage(key: &str) -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()??
        .get_item(key)
        .ok()?
}

fn write_storage(key: &str, value: &str) {
    if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = s.set_item(key, value);
    }
}

fn default_name() -> String {
    let n = (js_sys::Math::random() * 1000.0) as u32;
    format!("anon-{n}")
}

/// Pick a stable, vivid `#rrggbb` from a pubkey. Hue spread across
/// pk[0]; full saturation + medium lightness keeps colors readable on
/// the dark canvas without being washed out.
fn color_from_pk(pk: &[u8; 32]) -> String {
    let hue = (pk[0] as f32) / 256.0 * 360.0;
    let (r, g, b) = hsl_to_rgb(hue, 0.75, 0.55);
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to_byte = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to_byte(r1), to_byte(g1), to_byte(b1))
}

/// Allow only the `#rrggbb` shape we emit ourselves. Used both for
/// datachannel-frame colors and for contract-payload colors — neither
/// source is locally trusted, so both go through the same gate before
/// being passed to a CSS color sink.
fn sanitize_color(s: &str) -> String {
    let trimmed = s.strip_prefix('#').unwrap_or(s);
    if is_hex6(trimmed) {
        format!("#{trimmed}")
    } else {
        "#ffffff".to_string()
    }
}

fn is_hex6(s: &str) -> bool {
    s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn input_value(e: InputEvent) -> String {
    e.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

fn on_input(state: &UseStateHandle<String>) -> Callback<InputEvent> {
    let state = state.clone();
    Callback::from(move |e: InputEvent| state.set(input_value(e)))
}

fn short_pk(pk: &[u8; 32]) -> String {
    let s = hex::encode(pk);
    short_str(&s)
}

fn short_str(s: &str) -> String {
    if s.len() <= 12 {
        s.to_string()
    } else {
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    }
}

fn main() {
    wasm_logger::init(wasm_logger::Config::default());
    yew::Renderer::<App>::new().render();
}
