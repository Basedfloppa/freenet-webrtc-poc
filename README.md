# freenet-webrtc-poc

Collaborative drawing canvas between browser tabs, with offer/answer
+ trickle ICE signaled through a Freenet contract. Once WebRTC ICE
settles, pen strokes flow over a direct datachannel between the two
browsers — the contract is no longer in the hot path and the canvas
is **invisible to anyone not in an active call**.

## What this proves

- Freenet contracts work as a **WebRTC signaling channel** (SDP
  offer/answer + trickle ICE) over the same signed-entry LWW slot
  pattern used by `freenet-net-graph/topology-contract`.
- Browser tabs on different Freenet nodes can establish a direct
  datachannel; strokes show up on the other side as fast as the
  network round-trip allows.
- The signaling round-trip uses subscribe-push, not polling — so
  setup latency is bounded by Freenet ring latency, not by a polling
  interval.
- **Application data (strokes) never enters the contract.** The
  contract carries only presence + offer/answer/ICE/hangup. Even a
  random subscriber that's not in any call sees the peer list, but a
  blank canvas. Drawings are scoped to peers an active datachannel
  is open with.

> **Why drawing instead of camera/mic:** `navigator.mediaDevices` is
> `undefined` inside a Freenet webapp contract's null-origin sandbox
> iframe (the spec gates MediaDevices behind "secure context").
> RTCDataChannel has no such restriction, so a drawing app exercises
> the same WebRTC code paths (offer/answer, trickle ICE, NAT traversal,
> datachannel) without bumping into the secure-context gate.

## Layout

```
freenet-webrtc-poc/
├── shared/                  # Wire types: SignedEntry, SignalPayload, LWW + prune
├── signaling-contract/      # WASM contract — same shape as topology-contract
├── frontend/                # Yew SPA
│   ├── src/
│   │   ├── main.rs          # App component, peer list, draw canvas, log
│   │   ├── peer.rs          # RTCPeerConnection wrapper (datachannel-only)
│   │   ├── signaling.rs     # contract subscribe/publish over WS
│   │   └── ws_shim.rs       # null-origin-safe WebSocket (copied verbatim from net-graph)
│   ├── Trunk.toml           # serves on :9001 by default
│   ├── index.html
│   └── style.css
├── DEPLOYMENT.md            # Operator handbook with placeholders
├── DEPLOYMENT.local.md      # Real IPs/paths/seeds — gitignored
└── .gitignore
```

## Build

```bash
# Contract WASM (raw — file starts with \0asm directly, no fdev packing)
( cd signaling-contract && cargo build --release --target wasm32-unknown-unknown )
# → signaling-contract/target/wasm32-unknown-unknown/release/webrtc_signaling_contract.wasm

# Frontend bundle
( cd frontend && trunk build --release )
# → frontend/dist/
```

## Tests

```bash
cargo test -p webrtc-poc-shared          # 6 — sign/verify, LWW, cross-key, prune, canvas round-trip
( cd signaling-contract && cargo test )  # 4 — empty/round-trip/order-independence/stale-prune
( cd frontend && cargo check --target wasm32-unknown-unknown )
```

## Running it (two-tab demo)

For local development without Freenet at all, see [Step 2 alt
(`trunk serve`)](#trunk-serve-standalone) below. For the real flow
through a Freenet node:

This PoC needs:

1. A running freenet-core node (≥ 0.2.55) with WebSocket API.
2. The signaling contract published to that node via `fdev publish`.
3. The webapp published as a website contract via `fdev website`.

### Step 1 — publish the signaling contract

```bash
printf '\x00\x00\x00\x00\x00\x00\x00\x00' > /tmp/empty-state.bin
fdev publish \
    --code signaling-contract/target/wasm32-unknown-unknown/release/webrtc_signaling_contract.wasm \
    --parameters <(head -c 32 /dev/urandom) \
    contract --state /tmp/empty-state.bin
```

`fdev publish` prints the deployed `instance_id` and `code_hash` (both
base58). Copy them — `frontend/src/main.rs` has constants
`DEFAULT_INSTANCE_ID` / `DEFAULT_CODE_HASH` you'll need to update.

> If contract code changes (i.e. `shared` or `signaling-contract` rebuilt),
> code hash changes → instance id changes. See `DEPLOYMENT.md` for the
> full publish + cache-invalidation flow on production gateways.

### Step 2 — publish the webapp

```bash
fdev website init webrtc-poc       # one-time, generates a signing key
( cd frontend && trunk build --release )
fdev website update --key webrtc-poc ./frontend/dist
```

Hit `http://<node-ip>:7509/v1/contract/web/<webapp-id>/` to load the
app.

### Step 2 alt — `trunk serve` standalone

If you don't want to deal with website contracts during dev:

```bash
( cd frontend && trunk serve )   # → http://127.0.0.1:9001/
```

Open that URL. The frontend will derive the WS URL from `window.location`
and try `ws://127.0.0.1:9001/v1/contract/command` — point it at your
freenet node by editing the `ws://` field in the Connect form (visible
once on first load) and the saved value persists.

### Step 3 — pick an identity, draw

The webapp **auto-connects on load**. The Connect form only shows if
the auto-connect fails — typically you'll just see "subscribed" and
the peer list right away. The very first load picks defaults; sticking
them in the form once is enough.

Identity is stored in the URL hash (`#k=<base58_seed>&n=<your_name>`).
That means:

- **Reload preserves identity.** The outer Freenet shell forwards the
  hash through the iframe rebuild, so refresh keeps your pubkey + name.
- **Same hash on different nodes = same identity.** Bookmark
  `http://node-a.../#k=…&n=Alice` and paste the same `#k=…` part onto
  `http://node-b.../` and you appear as the same publisher there.
- **Treat the seed like a password.** Anyone with `#k=…` can sign as
  your pubkey. URL-bar visibility is the trade-off for sandbox
  identity portability — see *Identity portability* below.

Open the URL in **two browser tabs** with different hashes (or no hash
on a fresh tab to auto-generate one). Each appears as a distinct
publisher in the contract.

### Step 4 — make a call, draw together

In one tab, click **Call** next to the other peer's row. The other tab
shows `📞 Incoming call from <name>` with **Accept / Reject** buttons.
Accept and the green "datachannel open" dot appears next to the peer's
name. Now drag on the canvas:

- **Pen** — draw lines in the current color/width.
- **Eraser** — true delete: hit-tests against committed strokes and
  drops anything within radius, including the other peer's strokes.
- **Fill** — paint the canvas a single color, atomic.
- **Clear mine** — wipes only your own strokes (per-sender).

Color picker + 12-color preset palette + brush-size slider (1–40 px)
in the toolbar. Eraser disables the color picker since width is the
only knob that matters.

Network path: stroke frames flow over the direct WebRTC datachannel
using Google's public STUN (`stun.l.google.com:19302`) for NAT
traversal. Strokes from the past are replayed via `S,…` and `f,…`
frames when a fresh datachannel opens, so a late joiner sees the
existing canvas — *only* of peers they're directly connected to.

## Architecture

```
Tab A                            Freenet node A         contract           Freenet node B            Tab B
─────                            ──────────────         ────────           ──────────────            ─────
 │  WS subscribe/update ──────────────────────► UPDATE                                                │
 │                                              ────► broadcast to subscribers                       │
 │                                                                       UpdateNotification ────────► │
 │                                                                                                    │
 │  click "Call"                                                                                      │
 │  ─ createDataChannel("draw")                                                                       │
 │  ─ createOffer + setLocalDescription                                                               │
 │  ─ outbox += DirectedSignal::Offer{ to: B }                                                        │
 │  WS update ────────► …                                                                             │
 │                                                                                       offer ─────► │
 │                                                                                       ondatachannel│
 │                                                                                       (Accept UI)  │
 │                                                                                       accept_offer │
 │                                                                                       outbox += Answer{ to: A }
 │                                                                                       WS update ─► │
 │ ◄───── answer                                                                                      │
 │  setRemoteDescription                                                                              │
 │                                                                                                    │
 │  ICE candidates trickle both ways through the contract                                             │
 │                                                                                                    │
 │  ░░░░ direct WebRTC datachannel ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ │
 │  pen / eraser / fill frames (b/m/e/c/f/B/M/E/S) flow direct                                        │
 │  contract no longer carries any drawing data                                                       │
```

## Datachannel wire format

Comma-separated text frames over the open datachannel. Lowercase =
pen-class (commit on `e`), uppercase = eraser (no commit, hit-test
on `E`).

```
b,<color_hex6>,<width>,<x>,<y>   begin pen stroke at fractional point ∈ [0,1]
m,<x>,<y>                        extend current pen stroke
e                                end pen stroke (commits if ≥2 points)

B,<width>,<x>,<y>                begin eraser stroke (color implicit = bg)
M,<x>,<y>                        extend eraser stroke
E                                end eraser → hit-test removes intersecting strokes

f,<color_hex6>                   atomic full-canvas fill
c                                clear all strokes from THIS sender only
S,<color>,<width>,<x>,<y>,…      snapshot stroke (on-open replay; one frame per stroke)
```

Coordinates are fractions of canvas size so the same payload renders
correctly regardless of each side's pixel dimensions. Sender pubkey
is implicit (the channel knows who it's from), so frames don't carry
an author field.

On `Peer::on_open`, the side that has history pushes its own committed
strokes to the new peer as a burst of `S` (pen) and `f` (fill) frames
after a 500 ms delay (so the callee's `onmessage` handler is wired by
the time the burst arrives).

## Contract wire format

Same shape as `topology-contract`:

- `State`        : bincode of `ContractState { entries: BTreeMap<[u8;32], SignedEntry> }`
- `StateDelta`   : bincode of `ContractDelta { entries: Vec<SignedEntry> }`
- `StateSummary` : bincode of `ContractSummary { known_timestamps: BTreeMap<...,u64> }`

Each `SignedEntry` decodes to a `SignalPayload`:

```rust
SignalPayload {
    public_key: [u8; 32],             // Ed25519 verifying key — also the contract slot key
    display_name: String,
    timestamp_ms: u64,                // LWW key
    outbox: Vec<DirectedSignal>,      // active offer/answer/ICE/hangup
    canvas_strokes: Vec<CanvasStroke>, // ALWAYS PUBLISHED EMPTY — see scope note below
}

DirectedSignal {
    signal_id: u64,                   // monotonic per publisher; receiver dedupes by (sender, id)
    target: [u8; 32],                 // recipient's pubkey
    kind: SignalKind,                 // Offer / Answer / IceCandidate / Hangup
}
```

Merge: per-publisher LWW by `timestamp_ms`. Cross-key interference is
impossible — the public key is embedded in the signed payload and the
contract verifies the Ed25519 signature on every `validate_state` /
`update_state`. Stale entries (more than 5 minutes behind the freshest
timestamp) are pruned on every update — closed tabs and disconnected
publishers don't accumulate forever.

## Identity portability

Identity (Ed25519 signing key) lives in the URL hash because the
sandboxed iframe has a fresh **opaque origin** on every load —
`localStorage`, IndexedDB, and cookies are all per-load and unsuitable
for persistence. The Freenet outer shell exposes a `__freenet_shell__`
postMessage protocol with a `type: 'hash'` op that lets the iframe
update the **outer** URL via `history.replaceState`; on the next
reload the shell forwards the outer hash back into the iframe URL,
closing the loop.

Format: `#k=<base58_seed>&n=<urlencoded_name>`. Both fields optional
(missing key = generate new + write back).

The same protocol also handles `type: 'title'` (browser tab text)
which is why this PoC's tab reads "Freenet Collab Draw" instead of
just "Freenet" — see `set_shell_title` in `main.rs`.

## Scope semantics — why your strokes don't leak

The signaling contract is **public to every subscriber**. Anyone who
knows the contract id can subscribe and receive every publisher's
heartbeat. If the canvas were stored in `canvas_strokes` (as an
earlier iteration of this PoC was), drawings would be visible to
arbitrary onlookers — a privacy mismatch with the "I drew this for
my friend" intent.

So: `canvas_strokes` is published always-empty, and
`on_snapshot` ignores the field entirely. Strokes flow only over the
WebRTC datachannel between two peers in an active call. The `S`/`f`
on-open replay (above) catches up new peers, but only with the
strokes drawn by the peer they just connected to.

Trade-off: a late joiner who calls only one author sees only that
author's strokes. To get the full mesh state, call all current
authors. A future enhancement would relay strokes through other
mesh members, but that introduces author-attribution complexity.

## Known limitations of this PoC

- ~~**One active call at a time.**~~ Mesh works:
  `peers: HashMap<peer_pk, Rc<Peer>>`, multiple concurrent calls.
- ~~**No stroke history sync.**~~ Replay on `dc.onopen` covers it.
- ~~**Auto-accept incoming calls.**~~ Accept/Reject banner now.
- ~~**Clear is unscoped.**~~ "Clear mine" now scoped per-sender;
  receivers also drop only their copy of that sender's strokes.
- **No reconnect logic.** If the WS drops, the user has to refresh.
- **No TURN.** Symmetric NATs (mobile carriers, some corporate
  networks) won't connect — only STUN-friendly NATs work. For real
  use, add a TURN server (operator-run on Freenet nodes is natural).
- **IP leak.** WebRTC ICE exposes both peers' IP addresses to each
  other. For privacy-sensitive use, force TURN-only
  (`iceTransportPolicy: 'relay'`).
- **Stale-pubkey signal-id dedup.** The receiver dedupes signals by
  `(sender_pk, signal_id)`. With persistent identity (URL hash) a
  reload restarts the sender's `signal_counter` at 0 while the
  receiver still holds the prior session's high water mark — new
  offers fall under the bar and are silently dropped until the
  receiver also reloads. Real fix: also dedup on `timestamp_ms`.
- **URL hash exposes the signing key.** It's in the address bar.
  Treat it like a password; don't share screenshots that show it.
  No clean alternative inside a null-origin sandbox iframe.

## Why this matters

This same pattern (contract-as-signaling + WebRTC datachannel)
generalizes far beyond drawing:

- **Realtime collaborative editing** — datachannel for cursor + ops,
  contract for persistent doc state (when the data should be public)
- **P2P games** — datachannel for input + state, contract for matchmaking
- **Voice chat rooms** — many-to-many with SFU pattern (one peer relays)
- **Low-latency CRDT sync** — datachannel for hot ops, contract for
  cold-start state

In each case Freenet provides what raw WebRTC lacks: **discovery,
identity, and persistence** without a centralized signaling server.
The scope-controlled "datachannel-only application data" pattern
shown here applies whenever the data is more sensitive than the
discovery channel it rides on.

## Operator handbook

Production deploy commands, gateway addresses, cache-invalidation
recipes, and identity seeds for repeatable e2e testing live in
[`DEPLOYMENT.md`](./DEPLOYMENT.md) — operator-specific values
(real IPs, machine paths, test seeds) are filled in via the
gitignored `DEPLOYMENT.local.md`.
