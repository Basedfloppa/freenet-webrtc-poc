# Deployment Reference

Operational handbook for publishing the webrtc-poc to Freenet nodes.
Mirrors the role `freenet-net-graph`'s topology files play — single
source of truth for contract IDs, paths, and the exact commands to
redeploy.

> Operator-specific values (real IPs, machine-local paths, test
> identity seeds) live in **`DEPLOYMENT.local.md`** which is
> gitignored. The placeholders below — `<orange-lan-ip>`, `<fdev>`,
> `<webapp-signing-key>`, etc. — are filled in there.

## Live deployment (2026-05-10)

### Signaling contract

| Field          | Value                                                                  |
|----------------|------------------------------------------------------------------------|
| Instance ID    | `ExXxjKbTNbNYQs42HefyNBGc3Nk9iBCxh1M2jZg7buNR`                         |
| Code hash      | `4adm1dj8gV3KtcarYgFKqp6rEVDRnzwRj8tTKE5EoYwh`                         |
| Parameters     | `<contract-params-file>` (32 random bytes; see local file)             |
| WASM source    | `signaling-contract/target/wasm32-unknown-unknown/release/webrtc_signaling_contract.wasm` |
| Bincode shape  | `ContractState { entries: BTreeMap<[u8;32], SignedEntry> }`            |
| Stale prune    | `MAX_STALE_MS = 5 min` — entries older than (max ts - 5 min) drop on each update |

### Webapp contract

| Field          | Value                                                                |
|----------------|----------------------------------------------------------------------|
| Instance ID    | `JCZAdouraT5oQrqrCtAHK1u5fVYyPvFkZKoM35o18CTf`                       |
| Signing key    | `<webapp-signing-key>` (path in local file)                          |
| Bundle         | `frontend/dist/` after `trunk build --release`                       |
| Current bundle | `frontend-2d594ebf02c9a1db.js` (URL-hash identity + shell title)     |
| Live URLs      | `http://<orange-lan-ip>:7509/v1/contract/web/JCZAdouraT5oQrqrCtAHK1u5fVYyPvFkZKoM35o18CTf/` |
|                | `http://<baka-ip>:7509/v1/contract/web/JCZAdouraT5oQrqrCtAHK1u5fVYyPvFkZKoM35o18CTf/`       |

### Identity portability

Frontend reads + writes `#k=<base58_seed>&n=<urlencoded_name>` in the
URL hash via the outer shell's `__freenet_shell__` postMessage
protocol — the only persistent kv store available inside Freenet's
null-origin sandbox iframe (localStorage is per-load-instance there).

To "log in" as the same identity on a different node, copy the URL
including `#k=…` to the new node's host. Same key → same pubkey →
same publisher → same contract entry visible everywhere.

Bookmark format:
```
http://<gateway>:7509/v1/contract/web/JCZAdouraT5oQrqrCtAHK1u5fVYyPvFkZKoM35o18CTf/#k=<your-seed-base58>&n=<your-name>
```

> **The seed after `#k=` is a private key.** Treat it like a password.
> Anyone with that string can sign messages as your pubkey. Test seeds
> for repeatable e2e runs are stored in `DEPLOYMENT.local.md`.

## Tooling

| Placeholder       | Purpose                                                            |
|-------------------|--------------------------------------------------------------------|
| `<fdev>`          | Local-built fdev 0.3.217+ (has `website` subcommand). The PATH-installed fdev 0.3.151 lacks it. |
| `<codehash-util>` | Tiny util that prints proper-case base58 of `blake3(wasm_bytes)`. fdev's debug log lowercases the hash, so don't copy from there. |

## Operator nodes

Two gateway nodes (orange + baka) at freenet-core 0.2.55. Real IPs
and SSH aliases listed in `DEPLOYMENT.local.md`.

## Build commands

```bash
# Contract WASM (raw — no fdev packing, file starts with \0asm directly)
( cd signaling-contract && cargo build --release --target wasm32-unknown-unknown )
# → signaling-contract/target/wasm32-unknown-unknown/release/webrtc_signaling_contract.wasm

# Frontend bundle
( cd frontend && trunk build --release )
# → frontend/dist/
```

## Publish commands

### Signaling contract → orange (also propagates to baka via DHT)

```bash
FDEV=<fdev>
PACKED=signaling-contract/target/wasm32-unknown-unknown/release/webrtc_signaling_contract.wasm

# Empty initial state — bincode of `ContractState::default()` is 8 zero bytes (BTreeMap len 0)
printf '\x00\x00\x00\x00\x00\x00\x00\x00' > /tmp/empty-state.bin

$FDEV --address <orange-lan-ip> --port 7509 network publish \
    --code "$PACKED" \
    --parameters <contract-params-file> \
    contract --state /tmp/empty-state.bin
```

> If contract code changes (i.e. `shared` or `signaling-contract`
> sources), code_hash and instance_id change too. Compute the new ones
> via `$FDEV get-contract-id --code "$PACKED" --parameters <contract-params-file>`
> for the instance_id, and `<codehash-util> "$PACKED"` for the code
> hash. Then update the constants in `frontend/src/main.rs`
> (`DEFAULT_INSTANCE_ID`, `DEFAULT_CODE_HASH`) and rebuild the
> frontend before pushing the webapp.

### Webapp → orange + invalidate caches on every gateway

```bash
FDEV=<fdev>

( cd frontend && trunk build --release )

$FDEV --address <orange-lan-ip> --port 7509 network website update \
    --key webrtc-poc-42faa1 \
    ./frontend/dist
# Expect "Error: PUT operation timed out" *after* "Server confirmed
# successful execution" — the local PUT lands; the timeout is just
# the network-broadcast wait, harmless.

# Wipe stale unpacked-webapp caches on every gateway that has hosted
# the contract before (otherwise they keep serving the old JS hash).
# See DEPLOYMENT.local.md for the SSH-alias one-liner.

# Trigger an *outer-path* GET on each gateway to repopulate the cache
# from DB. Hitting `?__sandbox=1` first errors with "Contract not
# cached yet" because that path expects an unpacked dir to already
# exist. See sandbox-iframe-ws-shim memory.
WEBAPP_ID=JCZAdouraT5oQrqrCtAHK1u5fVYyPvFkZKoM35o18CTf
for HOST in <orange-lan-ip> <baka-ip>; do
    curl -s -o /dev/null -w "$HOST: HTTP %{http_code}\n" \
        "http://$HOST:7509/v1/contract/web/$WEBAPP_ID/"
done
```

## Verification sanity check

```bash
WEBAPP_ID=JCZAdouraT5oQrqrCtAHK1u5fVYyPvFkZKoM35o18CTf
EXPECTED_BUNDLE=$(ls frontend/dist/ | grep -oE 'frontend-[a-z0-9]+\.js' | head -1)
echo "expected: $EXPECTED_BUNDLE"
for HOST in <orange-lan-ip> <baka-ip>; do
    SERVED=$(curl -s "http://$HOST:7509/v1/contract/web/$WEBAPP_ID/?__sandbox=1" \
        | grep -oE 'frontend-[a-z0-9]+\.js' | head -1)
    [ "$SERVED" = "$EXPECTED_BUNDLE" ] \
        && echo "$HOST: ✓ $SERVED" \
        || echo "$HOST: ✗ served $SERVED (expected $EXPECTED_BUNDLE)"
done
```

## Tests gate (run before any publish)

```bash
cargo test -p webrtc-poc-shared          # 6 — incl. canvas_strokes_round_trip + prune_stale
( cd signaling-contract && cargo test )  # 4 — incl. update_drops_stale_publishers
( cd frontend && cargo check --target wasm32-unknown-unknown )
```

## Known frictions (for next session)

- Sandbox iframe identity persists *only* through URL hash; cookies,
  localStorage, IndexedDB are all per-load opaque-origin. Document
  this when handing the URL to a non-developer user.
- `fdev website update` always errors out with "PUT operation timed
  out" after the local PUT lands. Treat as success; verify via the
  curl bundle-hash check above.
- Webapp cache directory invalidation on each gateway is a known
  freenet-core bug — see `webapp-cache-invalidation` memory.
