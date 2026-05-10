# freenet-webrtc-poc

Collaborative drawing canvas between browser tabs, signaled through a
Freenet contract. Once WebRTC ICE settles, pen strokes flow over a
direct datachannel — the contract is no longer in the hot path.

## What this proves

- Freenet contracts work as a **WebRTC signaling channel** (SDP
  offer/answer + trickle ICE) over the same signed-entry LWW slot
  pattern used by `freenet-net-graph/topology-contract`.
- Browser tabs on different nodes can establish a direct datachannel
  with sub-100ms latency once the handshake completes — strokes show
  up on the other side as fast as the network round-trip allows.
- The signaling round-trip uses subscribe-push, not polling — so
  setup latency is bounded by Freenet ring latency, not by a polling
  interval.

> **Why drawing instead of camera/mic:** `navigator.mediaDevices` is
> `undefined` inside a Freenet webapp contract's null-origin sandbox
> iframe (the spec gates MediaDevices behind "secure context").
> RTCDataChannel has no such restriction, so a drawing app exercises
> the same WebRTC code paths (offer/answer, trickle ICE, NAT traversal,
> datachannel) without bumping into the secure-context gate.

## Layout

```
freenet-webrtc-poc/
├── shared/                # Wire types: SignedEntry, SignalPayload, LWW merge
├── signaling-contract/    # WASM contract — same shape as topology-contract
└── frontend/              # Yew SPA: WebRTC + drawing canvas
    ├── src/
    │   ├── main.rs        # App component, peer list, draw canvas, log
    │   ├── peer.rs        # RTCPeerConnection wrapper (datachannel-only)
    │   ├── signaling.rs   # contract subscribe/publish over WS
    │   └── ws_shim.rs     # null-origin-safe WebSocket (copied verbatim from net-graph)
    ├── Trunk.toml         # serves on :9001 by default
    ├── index.html
    └── style.css
```

## Build

```bash
# Contract WASM (~17 KB after release+wasm-opt — same opt profile as topology-contract)
cd signaling-contract
cargo build --release --target wasm32-unknown-unknown
# → target/wasm32-unknown-unknown/release/webrtc_signaling_contract.wasm

# Frontend
cd ../frontend
trunk build --release
# → frontend/dist/
```

## Tests

```bash
cargo test -p webrtc-poc-shared       # 3 — sign/verify, LWW, cross-key isolation
(cd signaling-contract && cargo test) # 3 — empty/round-trip/commutativity
```

## Running it (two-tab demo)

This PoC needs:

1. A running freenet-core node listening on the WebSocket API
   (`ws://127.0.0.1:7509` by default).
2. The signaling contract published to that node via `fdev publish`.

### Step 1 — publish the contract

You need both an **instance id** (per-deployment, derives from
parameters) and the **code hash** (BLAKE3 of the WASM). Easiest path:
use `fdev publish` and read both back from its output.

```bash
cd signaling-contract
fdev publish \
    --code target/wasm32-unknown-unknown/release/webrtc_signaling_contract.wasm \
    --release contract \
    --state <(printf '\x00\x00\x00\x00\x00\x00\x00\x00')   # empty BTreeMap, length 0
```

`fdev publish` prints the deployed `instance_id` and `code_hash` (both
base58). Copy them — you'll paste them into the frontend's Connect form.

> If you change the code, the code hash changes. The instance id
> stays the same as long as the parameters don't change. For a
> multi-room PoC, vary the parameters per room.

### Step 2 — serve the frontend

```bash
cd frontend
trunk serve   # → http://127.0.0.1:9001/
```

Open the URL in **two browser tabs / windows / browsers**. Each tab
generates its own Ed25519 identity in localStorage on first load, so
they appear to the contract as two distinct publishers.

### Step 3 — connect both tabs

In each tab:

1. Paste the contract `instance id` and `code hash` from Step 1
   into the Connect form.
2. Optional: set a display name (defaults to a random `anon-N`).
3. Optional: pick a pen color (defaults to one derived from your pubkey).
4. Click **Connect**. The status pill turns "subscribed".

Both tabs now publish their presence into the contract. Each one's
peer list populates with the other's identity.

### Step 4 — place the call

In one tab, click **Call** next to the other peer's row. Watch the
log:

```
→ offer to abc123…ef
← answer from def456…ab
(several ICE candidates each way)
```

When ICE completes, drag on the canvas in either tab — strokes appear
on both sides in real time. Network path: stroke frames flow over a
direct WebRTC datachannel using Google's public STUN
(`stun.l.google.com:19302`) for NAT traversal. **Clear** wipes the
canvas on both sides.

## Architecture

```
Tab A                            Freenet node A         contract           Freenet node B            Tab B
─────                            ──────────────         ────────           ──────────────            ─────
 │  WS subscribe/update ──────────────────────► UPDATE                                                │
 │                                              ────► broadcast to subscribers                       │
 │                                                                       UpdateNotification ────────► │
 │                                                                                                    │
 │  click "Call"                                                                                      │
 │  ─ createDataChannel("cursor")                                                                     │
 │  ─ createOffer + setLocalDescription                                                               │
 │  ─ outbox += DirectedSignal::Offer{ to: B }                                                        │
 │  WS update ────────► …                                                                             │
 │                                                                                       offer ─────► │
 │                                                                                       ondatachannel│
 │                                                                                       accept_offer │
 │                                                                                       outbox += Answer{ to: A }
 │                                                                                       WS update ─► │
 │ ◄───── answer                                                                                      │
 │  setRemoteDescription                                                                              │
 │                                                                                                    │
 │  ICE candidates trickle both ways through the contract                                             │
 │                                                                                                    │
 │  ░░░░ direct WebRTC datachannel ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ │
 │  pen-stroke frames (b/m/e/c) flow direct, contract no longer in the hot path                       │
```

## Datachannel wire format

Once the channel is open, each tab sends comma-separated text frames:

```
b,<color_hex6>,<x>,<y>   begin a stroke at fractional point (x,y) ∈ [0,1]
m,<x>,<y>                extend current stroke
e                        end current stroke
c                        clear all strokes (broadcast from sender)
```

Coordinates are fractions of canvas size, so the same payload renders
correctly regardless of each side's pixel dimensions.

## Wire format

Same shape as `topology-contract`:

- `State`        : bincode of `ContractState { entries: BTreeMap<[u8;32], SignedEntry> }`
- `StateDelta`   : bincode of `ContractDelta { entries: Vec<SignedEntry> }`
- `StateSummary` : bincode of `ContractSummary { known_timestamps: BTreeMap<...,u64> }`

Each `SignedEntry` decodes to a `SignalPayload`:

```rust
SignalPayload {
    public_key: [u8; 32],     // Ed25519 verifying key — also the contract slot key
    display_name: String,
    timestamp_ms: u64,        // LWW key
    outbox: Vec<DirectedSignal>,
}

DirectedSignal {
    signal_id: u64,           // monotonic per publisher; receiver dedupes by (sender, id)
    target: [u8; 32],         // recipient's pubkey
    kind: SignalKind,         // Offer / Answer / IceCandidate / Hangup
}
```

Merge: per-publisher LWW by `timestamp_ms`. Cross-key interference is
impossible — the public key is embedded in the signed payload and the
contract verifies the Ed25519 signature.

## Known limitations of this PoC

- **One active call at a time.** Extending to mesh = `HashMap<peer_pk,
  Peer>` + per-remote in-flight stroke slot. The signaling layer
  already supports multi-target (each `DirectedSignal` carries `target`).
- **No stroke history sync.** Late joiners start with a blank canvas;
  they only see strokes drawn after they connect. Production would
  ship the local stroke buffer once the datachannel opens.
- **No reconnect logic.** If the WS drops, the user has to refresh.
  Production would auto-reconnect with exponential backoff.
- **No TURN.** Symmetric NATs (mobile carriers, some corporate
  networks) won't connect — only STUN-friendly NATs work. For real
  use, add a TURN server (operator-run on Freenet nodes is natural).
- **No call-state cleanup.** After hangup, the outbox keeps the last
  signaling messages until the next publish overwrites them. Fine for
  PoC; production would `drop_signals_for(target)` on hangup.
- **IP leak.** WebRTC ICE exposes both peers' IP addresses to each
  other. For privacy-sensitive use, force TURN-only (`iceTransportPolicy: 'relay'`).
- **No call ringtone / accept UI.** Incoming offers are auto-accepted
  whenever a peer hits Call. Production would prompt the callee.
- **Clear is unscoped.** A `c` frame from the remote wipes every
  stroke we hold (including our own). Production would scope clears
  per-sender so only the remote's strokes go away.

## Why this matters

This same pattern (contract-as-signaling + WebRTC datachannel)
generalizes far beyond video calls:

- **Realtime collaborative editing** — datachannel for cursor + ops,
  contract for persistent doc state
- **P2P games** — datachannel for input + state, contract for matchmaking
- **Voice chat rooms** — many-to-many with SFU pattern (one peer relays)
- **Low-latency CRDT sync** — datachannel for hot ops, contract for
  cold-start state

In each case Freenet provides what raw WebRTC lacks: **discovery,
identity, and persistence** without a centralized signaling server.
