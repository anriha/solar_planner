# Deye SUN-12K-SG05LP3-EU-SM2 — Solar Comms & HA Integration Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Get reliable Modbus RTU telemetry from the Deye inverter to the Raspberry Pi, expose it to Home Assistant bidirectionally (Pi owns control logic, HA provides overrides/UI), and leave a clean path to price-driven control.

**Architecture:** One Rust service on the Pi (`solar-bridge`) is the *sole* Modbus master on the RS485 bus, built as a **kameo actor system**. `BusActor` owns the serial port — single-master discipline is structural (all bus access serializes through one mailbox; no other task can even hold the client). It polls, decodes, and pushes snapshots to `StateActor`, which fans out to per-connection actors serving **one bidirectional WebSocket**. HA runs a small Python custom integration consuming that socket. No polling from HA, no MQTT, no cloud dependency for local data.

**Current scope decision (2026-09-27):** implement Phase 1 + Phase 2 *completely* — including Task 2.4 directives, so the write-path contract exists before the planner does. Phase 3 (price-driven control) stays future; no code writes registers until it lands.

**Tech Stack:** Bridge in Rust (tokio + tokio-modbus RTU master + tokio-serial + kameo actors + serde). Register map is a **TOML file** (`maps/sun_3ph_hybrid.toml`), compiled in via `include_str!` with an optional runtime path override. The HA-facing contract is the **JSON `meta` payload** serialized from the Rust types — no YAML anywhere. Deploy: **cross-compile on anriha-pc** (static aarch64 musl), scp, run under a plain systemd user service via a `justfile`. **No nix on the Pi.**

---

## Decisions log (2026-09-27 session)
1. **YAML is out.** Map lives in TOML; HA gets JSON meta over the WS. The original "shared YAML artifact" idea is dead — the only shared thing is the JSON schema of `meta`.
2. **kameo 0.22.2** for the actor system (verified healthy: updated 2026-07-18, ~887k downloads; core API read from vendored source in `~/.cargo/registry`). `kameo_actors` 0.8.1 available (pubsub/broker/message_bus modules exist; **their APIs have not been read yet — do so before using**).
3. **Deploy = cross-compile + justfile.** No nix on the Pi. Static musl binary (`aarch64-unknown-linux-musl`) via cargo-zigbuild or nix `pkgsCross` — decide at first build. Plain systemd user service on the Pi.
4. **HA is already on the tailnet** — former risk item closed.
5. **Directives (2.4) are in scope now** to freeze the write-path contract early.
6. **Planning values get a typed boundary:** `PlanningSignals` (newtypes) extracted from the snapshot; the planner never sees the 84-sensor diagnostic surface.
7. **base_word = LOW**, verified twice on hardware (see Verified section — and note the *original* capture in this plan was transposed; corrected below).

## Decisions log (2026-09-28 session)

1. **Partial poll failure = whole-cycle failure ("decision A").** Any span-read or sensor-decode error fails the poll; StateActor keeps the last good snapshot and flips `link_ok` only. Errors carry sensor name + address (coverage gap vs decode failure are distinct messages). Pinned by `tests/decode_all.rs::fails_loudly_when_a_span_is_missing`.
2. **`decode_all` reassembles via a flat address→word `BTreeMap`**, not per-span slicing — immune to a sensor ever straddling the 64-reg chunk boundary (586–679 splits at 650). `words_at` deleted (tests/spans now 8/8).
3. **Snapshot keyed by sensor name** — it's the meta↔state join key; `validate()` uniqueness makes it sound; HA entity `unique_id` = name. Address-keying saves zero bytes (JSON keys are strings anyway).
4. **systemd unit split out of Task 2.1 → Task 0.4** — no WS dependency, and the 24 h soak should run under `Restart=always`, not tmux.
5. **Backoff**: consecutive-failure exponent, cap 60 s; first retry paced by the regular ticker (no added delay) — avoids off-by-one tick-skips.
6. **`top` dropped** (2026-09-28): `dump` + journald cover debugging; a live table was speculative debug surface.
7. **axum 0.8.9** for the WS surface (2026-09-28, Phase 2): one router serves `/ws` upgrade + `GET /state /meta /directives`; tokio-tungstenite-direct rejected (hand-rolled upgrade + second listener for debug HTTP).
8. **Fan-out = plain mpsc subscribers** in StateActor (NOT per-connection kameo actors, NOT kameo_actors pubsub — risk #8 closed as unused). Each connection: reader loop in the handler + one writer task owning the socket sink; all frames serialize through the writer via an mpsc channel. Dead/lagging subscriber = pruned on `try_send` failure.
9. **Hello before Subscribe**, sent on the un-split socket: a push missed in the gap self-heals at the next tick — every state push carries full values + directives.
10. **DirectivesActor commit order:** validate → persist → notify StateActor → commit memory. Persist failure rejects the cmd; corrupt `directives.json` refuses startup (loud). `version` is a change counter — no-op cmds ack ok with no bump/persist/push.
11. **Protocol addition:** `{"type":"error","message":…}` server→client for malformed input (silent ignore would kill bring-up debugging).
12. **`run` requires `-c` and a non-empty token** — refusing to bind 0.0.0.0 unauthenticated is a startup error.

## Decisions log (2026-10-11 session)

1. **HACS custom-repository installability done** (not the default store): `hacs.json` added, manifest `documentation`/`issue_tracker` → anriha/solar_planner, manifest `version` 0.0.1, config_flow placeholder crash fixed (logger). Tagged `v0.0.1`, installed from HACS on home.anriha.space — working.
2. **Task 2.2 scope extended (decision 1b):** directive entities (switch/select/number) AND control readbacks as read-only sensors. Register writes stay Phase 3-only — unchanged (superseded by #8 below, same session).
3. **Directive entity schema ships in meta (decision 2b):** Rust emits a top-level `meta.directives` array; HA generates all directive entities from it, zero hardcoded names. Types are data-typed (`bool`/`enum`/`number`), not HA widgets. Exact JSON contract frozen in the `src/directives.rs` contract comment. Ranges mirror the Percent/Price newtypes but are UX clamps only — `apply()` stays the validation authority.
4. **No clear-from-HA for nullable directives in v1 (decision 3c):** null renders as unknown; clearing stays a websocat cmd; button entities later if actually needed.
5. **All entities on the existing Deye device** (same `{serial}` identifier).
6. **HA write path:** `hub.async_cmd` — uuid id, pending-future map resolved by `cmd_ack` (ok=false → error surfaced in UI), pending failed on connection drop, 10 s ack timeout. Entity display always from server pushes — never optimistic. Directive entity availability gates on WS connectivity only (directives stay valid when link_ok=false); sensors still gate on link_ok.
7. **Division of work:** HA/Python side fully delegated to the agent (complete, in working copy); Rust changes scaffolded with DECIDE holes for the human: (a) DirectiveSpec representation + placement, (b) meta composition (typed envelope vs json merge), (c) controls decode (generalize decoder input vs second loop) + cross-kind name uniqueness in `validate()` (values is name-keyed; sensors∪controls dupes would silently overwrite) + `tests/decode_all.rs` coupling coverage for controls.
8. **Plan amended (D1): register writes pulled forward to now, HA as writer.** Was: "planner is provably the first writer" (2026-09-27 #5/Phase 3). Now: HA-writable control registers as a *debug tool* (rare manual writes, testing aid); planner directives stay but are secondary until Phase 3. Consequence: the future planner must treat registers as source of truth (read-before-write, no shadow state).
9. **Write protocol (D2): dedicated `write`/`write_ack` frames** — `{"type":"write","id","name","value"}` / `{"type":"write_ack","id","ok","error"?}`. Directives keep `cmd` untouched; DirectivesActor never sees register writes. BusActor's mailbox serializes writes with polls (single-master discipline preserved).
10. **Writable surface (D3): `writable = true` on ALL 64 active controls for now** — schema field is deny-by-default (`#[serde(default)]`), but the map sets true everywhere; 172–177/340/186 semantics still unresolved, so writes there are at the user's own risk until LCD-verified. Tightening later = flipping map entries.
11. **Validation (D4): out-of-range writes REJECTED, never clamped** — bridge is the authority; HA UI clamps are cosmetic.
12. **Readback (D5): FC06 → FC03 → raw equality before ok=true**; mismatch = error ack.
13. Open holes for the Rust fill: select-write value semantics (label vs u16 value), validation placement (server task vs BusActor), write-encoder placement + number representability policy — plus round-1 holes (directive_schema body, meta composition, controls decode, validate() cross-kind uniqueness, decode_all coverage).
14. **Write-path decisions (second batch):** select writes carry the option **u16 value** (1b — labels stay display-only, no new label-uniqueness invariant); **BusActor owns validation/encoding** (2b — ws reader stays thin, encoder lives bus-side as pure `encode_write`); **non-representable numbers rejected** (3a — 1e-6 float-artifact tolerance on `(value)/scale + offset`, genuine non-integers reject); **shared `Field` decode input** (4a — one decoder for SensorDef/ControlDef, controls' Select constraints feed the options path so enum readbacks decode to labels).
15. **Write path implemented:** `encode_write` (reject-never-clamp, s16 two's-complement wrap, writable gate, u16/s16-only) + `WriteRegister` message (FC06 with 2 s timeout → FC03 readback → raw equality; IO failure = record_failure + error ack; readback mismatch = loud error, link NOT flipped) + server routing via `AppState.bus` + `validate()` folds sensors∪controls into one name set. Tests: tests/write.rs (9: switch/select semantics, range reject, representability, offset roundtrip anchor, s16 wrap, gates, full real-map roundtrip), decode_all coupling covers controls, map_parse cross-kind duplicate.
16. Remaining: hardware bring-up verification of the write path (controlled window, LCD cross-check per plan gate), then `just deploy`, HA reload, tag `v0.0.2`.
17. **Independent review of the write path (pre-deploy):** fixed — controls `scale == 0` now rejected in `validate()` (NaN would silently write raw 0); protocol.rs contract comment updated to the settled select-u16 semantics; successful write+readback resets `fails`/`next_attempt` (polls no longer skip up to 60 s after a write proves the link); hub send-race OSError → BridgeCmdError; ControlEntity availability gates on `link_ok` too (reads AND writes are bus-dependent — unlike directives); ControlSwitch renders non-{on,off} raws as unknown; `_state_from` directives param required (dead default branch). **Open for the hardware window:** FC03 readback fires immediately after FC06 — Deye may serve a stale register on an eager readback; if spurious Readback errors show up, add a 50–200 ms settle delay (measure first, don't guess). Full suite 68/68 green.

## Engineering gotchas discovered (do not re-learn these)

- **TOML integer keys don't deserialize into `BTreeMap<u16, _>`** ("invalid type: string, expected u16" — TOML keys are strings by spec). Options are therefore **arrays of `{ value, label }`** (`EnumOptions`). Same shape on the JSON meta.
- **serde `deny_unknown_fields` does NOT fire inside internally-tagged enum variants** (verified live: `base_word` on a u16 entry was silently dropped). Fixed via deny-strict raw mirrors (`RawEncoding`, `RawConstraint`) + `deserialize_with` conversions that enforce field combinations and reject cross-kind fields. Bonus: errors carry TOML span positions. If a new tagged enum is ever added to the schema, it needs the same treatment.
- **`#[serde(default)]` on `f64` yields 0.0, not 1.0** — scale uses `default = "unit"`.
- **crane's `cleanCargoSource` drops `maps/`** — `include_str!("../maps/...")` breaks under `nix build`. The flake `src` filter must be widened when the cross-build is wired up. (Local `cargo` builds are unaffected.)
- **Block reads:** measured span layout from `plan_spans` @ MERGE_GAP=8: **12 FC03 spans** — (3–7) (20–21) (80) (99–162) (163–191) (209–220) (340) (500) (514–541) (555–558) (586–649) (650–679) — sensors ∪ control read-backs, gaps ≤8 dead regs bridged (99–162 and 163–191 are one 93-reg run chunked 64+29; without bridging it's 37 requests). 586–679 is 94 regs → chunked **≤64 regs per request**. Timing at 9600 8N1: ~1.15 ms/char, a 64-reg round-trip ≈ 200 ms incl. turnaround → 12 blocks ≈ 2–3 s per cycle. Fits the 5 s interval; don't go below 5 s without re-measuring.
- **kameo `ask` flattens `Result` replies:** with `type Reply = Result<T, E>` you get `Result<T, SendError<M, E>>` — the handler's error arrives as `SendError::HandlerError(e)`, no nested match.
- **axum `WebSocket::recv` exists only pre-split**; after `split()` use `StreamExt::next` / `SinkExt::send`.
- **Internally-tagged wire enums need `rename_all = "snake_case"`** or tags serialize as variant names (`"Hello"`, not `"hello"`) — and deserialization of `"cmd"` silently falls into the error arm. Caught by the e2e test, not by the compiler.

## Crate pins (checked 2026-09-27, all alive)

`toml` 1.1.6 · `kameo` 0.22.2 · `kameo_actors` 0.8.1 · `tokio-modbus` 0.17.0 · `tokio-serial` 5.5.0 · `thiserror` 2 · `anyhow` · `clap` 4 (derive). `serde_norway` was considered and dropped (no YAML anymore). Prefer rustls over openssl if TLS ever appears (it shouldn't — plain `ws://` inside the tailnet), keeps the static binary clean.

---

## Context / verified facts

- Pi: hostname `solar`, `konazumi@100.64.0.3` (tailnet), Raspberry Pi OS trixie, Python 3.13.5, **no nix (by decision)**. `/dev/ttyUSB0` present, `konazumi` in `dialout`, `mbpoll` installed.
- HA: 192.168.1.187:8123 (home.anriha.space), **on the tailnet**. MQTT/Node-RED exist but rejected (see Phase 2 rationale).
- Inverter: SUN-12K-SG05LP3-EU-SM2 = three-phase low-voltage hybrid (SG05LP3 family).
- **Critical gotchas from field reports (still valid):**
  1. Three-phase SG05LP3 uses a **different register map** than the 1-phase SG03/SG04 docs. Use this repo's `maps/sun_3ph_hybrid.toml`.
  2. **Do NOT poll the BMS 485/CAN port** — it collides with battery comms. Use the dedicated Modbus port. (Reading the *BMS mirror* registers 210–220 is safe — that's the inverter's own cached copy.)
  3. COM port may default to master mode (meter driving). Ours is confirmed in slave mode.
  4. Serial: **9600 8N1, slave ID 1**, live values and settings both in holding registers, FC03 read / FC06-FC16 write.
  5. Many registers signed + scaled; wrong parsing = garbage. Sign conventions still open (see Risks).
  6. SolarMAN WiFi stick uses its own logger port; coexists. Only one Modbus master on the RS485 bus (enforced structurally by BusActor).
- Matter: rejected (see original assessment — HA support immature, full bridge stack needed, can't express TOU/grid-charge semantics).

**Register-map references:** `github.com/kellerza/sunsynk` (three_phase_common.py, three_phase_lv.py), `github.com/Developer089/deye-modbus-ha` (sun_3ph_hybrid.yaml), official Deye PDF. Our TOML was seeded from these and then **corrected against live hardware** — where sources conflict with our captures, the captures win.

---

## Phase 0 — Build & deploy environment (anriha-pc + Pi)

### Task 0.1: User in dialout group — DONE 2026-09-27
### Task 0.2: ~~Rust toolchain via nix on the Pi~~ → REPLACED: cross-compile on anriha-pc — **DONE 2026-09-27 (pkgsCross route)**
- Decision: **pkgsCross + `aarch64-unknown-linux-musl`** (cargo-zigbuild rejected — keeps the build inside the existing nix/crane graph). Verified: `nix build .#solar-bridge-arm` → `file` reports static aarch64 ELF.
- **Gotcha (rust-overlay × crossSystem):** applying rust-overlay inside the cross pkgs dispatches on hostPlatform → aarch64 toolchain *binaries* → `exec format error`. Toolchain must come from **nativePkgs** with `targets = [ "aarch64-unknown-linux-musl" ]`; the cross pkgs (no overlay) only feed `crane.mkLib` for the musl linker wrapper. (Same trap presumably applies to the qbittorrent_exporter pattern if its aarch64 image is ever built on x86_64.)
- crane `src` filter widened (`filterCargoSources || maps/*.toml`) — `include_str!` survives `nix build`; both native and cross builds green.
- `justfile`: `test` / `build` / `build-arm` / `deploy` (scp + atomic mv; systemd restart lands with Task 2.1) / `probe` (ssh passthrough). Pi coordinates centralized in the justfile header.

### Task 0.3: Stable serial device name on the Pi — SUPERSEDED 2026-10-04 (hardware change, rule retired)
Was: udev symlink `/dev/ttyRS485` for the CH340 USB dongle. Now: inverter connected via an SC16IS752 SPI-to-UART bridge (dmesg: `spi1.0 ... is a SC16IS752`) → `/dev/ttySC0` (channel 0; `ttySC1` = channel 1, unused). Numbering is fixed by the dtoverlay — deterministic across boots, no udev rule needed; there is no idVendor/idProduct on SPI. Rule file and `just install-udev` deleted from the repo. Pi was reinstalled 2026-10-04; the new install never had the rule.

### Task 0.4: systemd user unit — READY (handed over 2026-09-28, split from Task 2.1)
`systemd/solar-bridge.service` (user service, `Restart=always`); `just install-unit` ships + enables. Needs `loginctl enable-linger konazumi` once (user service without a session). journald timestamps stderr — no in-code log timestamps needed.

---

## Phase 1 — Modbus communication working (Goal 1)

### Task 1.1: Wiring + link check — DONE 2026-09-27 (mbpoll reads OK, slave mode confirmed)
### Task 1.2: Smoke test identity — **DONE 2026-09-27** (mbpoll first, then Rust `probe` reproduced SN 2605096069 on the Pi — first Rust↔hardware contact; tokio-modbus risk #6 retired for reads)
### Task 1.3: Register map file — **DONE**
`maps/sun_3ph_hybrid.toml`: 90 sensors, 64 controls. Every entry cross-checked against Developer089 + sunsynk and anchored to hardware captures (see comments inline). Notables:
- `base_word = "low"` on all 32-bit entries (hardware-verified twice).
- `serial_number` str/5 words @3; `rated_power` u32 @20; faults 555–558 raw; BMS mirror 210–220; battery-2 block included.
- Controls incl. TOU 148–177, SOC thresholds 115–117, current limits 108/109 (max 240 per live readback), grid protection 185/186 (widened, semantics unverified).
- Disabled: `active_power_regulation` @77 — reads 0xFFFF (unset sentinel; needs a `sentinel` schema concept before exposure).
- PV3/PV4 skipped (12K has 2 MPPTs).
- 2026-09-28 additions (readback-verified): UPS phases 640–642, inverter phases 633–635, battery V thresholds 99–101/118–120, charge starts 123–127, plus 133/135/137, 141, 191, 209, 104.
### Task 1.3b: Typed loader + strict validation — **DONE**
`src/map.rs`: `RegisterMap`/`SensorDef`/`ControlDef`/`Encoding`/`Constraint`/`EnumOptions`, raw-mirror strict deserialization (see gotchas), `validate()` (unique names, options⇒u16, scale≠0, min<max, on≠off). `load_from_str` + `load(path)` + `EMBEDDED_MAP`. Tests: `tests/map_parse.rs` — **13/13 green**, incl. real reject-fixtures (TOML, not the old vacuous YAML ones) and corrected anchors.
### Task 1.4: Span planner + decoding — **DONE**
- `src/decode.rs`: DONE (tests/decode.rs 11/11).
- `tests/decode.rs`: 11 vectors = the contract. Anchors: 0x1545→54.45 V; 0xFFF6→−10 (s16); u16 never sign-extended (0xFFFF→65535); temp (1165−1000)×0.1→16.5; **u32 base-low**: (534,535)=(0x022E,0x0000)→55.8 kWh and (20,21)=(0xD4C0,0x0001)→12 000 W; base-high mirror assembly-only; s32 sign across words; options 2→"normal", 7→UnknownOption(7); SN words→"2605096069"; word-count error.
- `src/spans.rs`: DONE — `plan_spans(map, merge_gap)` + `MERGE_GAP = 8`. Single `chunk_by` pass over the deduped sorted address set, predicate `b − a ≤ merge_gap + 1` (adjacent diff = dead regs + 1, so G=0 = pure contiguity), runs chunked ≤64 regs via `step_by`. Real map @ G=8 → **12 spans** (largest: 586–679, 94 regs → 64+30; settings run 99–191, 93 regs → 64+29); without bridging it would be 37 requests/cycle. `tests/spans.rs` 8/8: exactly-once coverage, G=0 exact-coverage property, sorted/disjoint/fully-bridged, chunk limit, hard 12-span layout anchor.
- `words_at` — REMOVED 2026-09-28: `decode_all` builds a flat address→word map instead (see decisions 2026-09-28 #2).
### Task 1.5: Robustness — **DONE (code)** 2026-09-28; Pi-side setup + soak pending
Backoff: consecutive-failure exponent in BusActor (skip ticks until `next_attempt`; cap 60 s; first retry paced by the ticker — `backoff_delay` pure fn). `src/config.rs` (port, baud, slave, interval_secs, bind, token — strict TOML, `deny_unknown_fields`). CLI: `-c/-p/-s/-b` are **global** flags, computed once before dispatch; `-c` wins wholesale over flags. `link_ok` in every snapshot; last-success timestamp = `latest.ts` (derived). Example: `config.example.toml` — real config only on the Pi, never committed (token).
### Task 1.6: CLI — **DONE** (probe + dump on hardware). `top` **DROPPED** 2026-09-28: `dump` + `just logs` (journald) cover debugging; a live table was speculative surface.
### Task 1.7: Actor skeleton — **DONE 2026-09-28** (first hardware run via `dump`; kameo 0.22.2 core API verified from vendored source — risk #8 retired for core kameo)
`src/bus.rs`: kameo `BusActor` — owns tokio-serial + tokio-modbus RTU client; interval poll; decode; `tell`s snapshot to StateActor. **No write handler in this phase** — the mailbox accepts reads' results only; the write message type arrives with Phase 3 so the planner is provably the first writer.
`src/state.rs`: `StateActor` — caches latest snapshot + link_ok; subscriber fan-out (plain Vec of ActorRefs or kameo_actors pubsub — read its API first).

---

## Phase 2 — Export to Home Assistant (Goal 2)

### Decision: Pi pushes state; HA sends commands (unchanged)
Rejected alternatives (for the record): HA RESTful polling (ownership inversion), Pi POSTs to /api/states (orphaned entities), MQTT discovery (user preference), ser2net + HA modbus (violates single-master), Matter (immature + wrong semantics). Chosen: one bidirectional WS, bridge pushes, entities native in HA.

### Task 2.1: Bridge WS server — **DONE 2026-09-28** (code + e2e `tests/server.rs`; deployed to the Pi, websocat-verified on hardware)
axum (`src/server.rs`): `/ws` upgrade (Bearer header or `?token=`) + `GET /state|/meta|/directives` on one router. Per-connection: reader loop + writer task over a split socket; pushes via mpsc subscribers in StateActor. hello/state/cmd/cmd_ack/ping/pong + `error` (see decisions #7–9, #11).
Protocol (JSON, unchanged):
```
server→client  {"type":"hello","meta":{...},"state":{...},"directives":{...}}
server→client  {"type":"state","ts":...,"link_ok":true,"values":{...},"directives":{...}}
client→server  {"type":"cmd","id":"c1","name":"force_mode","value":"charge"}
server→client  {"type":"cmd_ack","id":"c1","ok":true,"directives":{...}}
client→server  {"type":"ping"} / server→client {"type":"pong"}
```
- `meta` = serde-serialized `RegisterMap` (+ controls + directives schema) — **JSON from Rust types, map-driven HA entities, zero hardcoded addresses**.
- **Commands never write Modbus.** A `cmd` mutates directive state only. The server task has no write path to the bus by construction.
- Directive state versioned, persisted (`directives.json`), echoed in `hello`/`cmd_ack`/`state`.
- HTTP `GET /state|/meta|/directives` for curl-debug. ~~systemd user service on the Pi~~ → unit landed as Task 0.4.
Expected: `websocat` sees hello+stream; a manual `cmd` flips a directive and persists across restart.

### Task 2.2: Custom HA integration — HA side DONE 2026-10-11 (in working copy, untagged); bridge meta/decode extension scaffolded, Rust holes open
`custom_components/solar_bridge/` (Python, aiohttp WS client, auto-reconnect, entities from `meta`; options arrive as `{value,label}` arrays). `link_ok` drives availability; one HA device; state_class per meta → Energy dashboard native. Install via `ssh hassio@192.168.1.187` or Samba addon.
### Task 2.3: Energy dashboard readiness — OPEN
Cumulative registers exposed `state_class: total_increasing` (already set in the map); wire solar/grid/battery in/out; sanity-check 24 h curves.
### Task 2.4: HA → Pi directives — **DONE 2026-09-28** (contract frozen; consumed only in Phase 3)
`src/directives.rs`: typed `Directives` (version counter, ForceMode enum, `Option<Percent>` 0–100, `Option<Price>` sign-free ±10k) + `ApplyError`, atomic `directives.json` persistence (corrupt file refuses startup), `DirectivesActor` (persist-before-commit, no-op cmds change nothing). Tests: `tests/directives.rs` 10/10.
Directive schema: `planner_enabled` (bool), `force_mode` (auto|charge|discharge|hold), `soc_target_hold` (0–100|null), `price_override` (CZK/kWh|null). Validated against schema, persisted, acked with full state. Consumed only in Phase 3 — contract frozen now. HA side: switch/select/number entities from meta's directives section; displayed state from pushes, never optimistic.

---

## Phase 3 — Price-driven control (sketch, future)

- `PlannerActor`: consumes `PlanningSignals` + directives + OTE hourly prices; decides charge/discharge windows.
- **Sole Modbus writer.** Translates decisions into TOU writes (148–177), work mode (142), grid-charge (130). Safety limits are hard clamps in the write path (max charge power from `rated_power`/limits, SOC floor/ceiling, temperature gating, shutdown ≤ low ≤ restart ordering for 115–117), not suggestions. BusActor grows the write handler here — and only here.
- Recompute on price update + directive change; write only on plan delta.
- Before ANY first write: controlled window, one register at a time, readback-verified, LCD cross-checked. Resolve the 172–177 bitfield-vs-select question and the 340 naming conflict first (see Risks).

### The planning boundary (design fixed 2026-09-27, `src/telemetry.rs` — OPEN)
```rust
pub struct Snapshot { pub link_ok: bool, pub ts: u64, pub values: BTreeMap<String, Value> } // full diagnostic surface (HA/meta)
pub struct Watts(pub f64); pub struct Percent(pub f64); pub struct Celsius(pub f64);
pub enum RunState { Standby, SelfCheck, Normal, Alarm, Fault }
pub struct PlanningSignals { run_state, battery_soc, battery_power, battery_temperature,
                             pv1_power, pv2_power, pv_power_total /* derived */, load_power, grid_power }
impl PlanningSignals { fn extract(&Snapshot) -> Result<Self, ExtractError> } // fails LOUDLY on missing/renamed sensor
```
Coupling test: synthesize a full register image over the real map (zeros, first enum index for options), decode all sensors, assert extract succeeds — the CI-checked contract between the 84-value surface and the planner view.

---

## Files (current + planned)

```
Cargo.toml  flake.nix  justfile  config.example.toml
maps/sun_3ph_hybrid.toml       # register map — DONE, 84 sensors / 46 controls
src/main.rs                    # CLI: probe | run | dump (global -c/-p/-s/-b flags)
src/lib.rs
src/map.rs                     # types + strict loader + validate — DONE
src/decode.rs                  # decode — DONE (11/11)
src/spans.rs                   # span planner (plan_spans, MERGE_GAP) — DONE
src/config.rs                  # Task 1.5 — DONE (strict TOML + defaults)
src/telemetry.rs               # DONE — Snapshot (PlanningSignals lands with Phase 3)
src/bus.rs                     # DONE — connect/read_span/poll_once/decode_all/BusActor + backoff
src/state.rs                   # DONE — StateActor (Update, GetState/Status)
src/protocol.rs                 # DONE — wire types (StatePayload, ServerMsg, ClientMsg)
src/server.rs                   # DONE — axum WS + HTTP debug (2.1)
src/directives.rs               # DONE — directive schema, validation, persistence, actor (2.4)
tests/decode.rs                # 11-vector contract — green
tests/map_parse.rs             # 13 strict-loading tests — green
tests/spans.rs                 # 8 span-layout tests — green
tests/decode_all.rs            # 2 — synthetic-image coupling + loud failure — green
tests/backoff.rs, tests/config.rs, tests/directives.rs, tests/server.rs # DONE — green (4/5/10/3)
systemd/solar-bridge.service   # Task 0.4 — in repo; Pi install pending (just install-unit + linger)
# on the HA box: /config/custom_components/solar_bridge/
```

## Validation summary

- Phase 0: `just build-arm` produces static aarch64 binary; `just deploy` runs it on the Pi.
- Phase 1: `cargo test` green (map 13/13, decode 11/11, spans 8/8, decode_all 2/2); `probe` prints SN 2605096069 from Rust; `dump` produced a full snapshot on the Pi 2026-09-28; remaining: 24 h soak under the systemd unit surviving dongle replug (`top` dropped — see decisions #6).
- Phase 2 (2026-09-28): bridge side green — 43/43 tests incl. WS e2e (hello/ping/cmd→ack+push/persist/auth-reject); `nix build .#solar-bridge-arm` static; deployed, websocat hello + stream verified from the tailnet. Pending: 2.2 HA integration entities live over WS, unavailable within a minute of bridge stop; Energy dashboard curves after 24 h; HA toggle → cmd → ack → directive persisted across restart.
- Sign conventions confirmed under real import/export and charge/discharge before Phase 3 trusts them.

## Verified on hardware (2026-09-27)

- Link: Modbus port, RTU 9600-8N1, slave 1, FC03 OK; no process holding the port.
- Serial number ASCII at holding regs 3–7 → "2605096069". Run state (500) = 2 "normal".
- **Word order = base register holds the LOW word. VERIFIED TWICE:** rated power (20,21)=(0xD4C0,0x0001) → 0x0001D4C0=120000 → 12 000.0 W (independent ground truth: it's a 12K); PV total (534,535)=(0x022E,0x0000) → 558 → 55.8 kWh. ⚠️ **The original session note listed (534,535) as (0x0000,0x022E) — that pair was TRANSPOSED and briefly led us to implement base=high. Do not trust old captures; these two are the anchors.** Matches Developer089 `low_high` and sunsynk assembly.
- Battery V 587 (0x1545→54.45 V), SOC 588 (100 %), current 591 (s16, +0.07 A), temp 586 (offset 1000 → 16.5 °C); PV1/2 at 676-679/672-673 (V×I=P ✓); inverter V 627-629 (227.7/230.0/230.0), freq 638 (50.00 Hz), inverter power 636 = load power 653 = 2001 W; temps 540/541 (25.0/38.5 °C).
- Energy: PV today 529 (28.1 kWh), PV total 534/535 (55.8), batt 514-519, load 526/527.
- Control read-backs: 108=109=200 A (device default → constraint max 240); 185=276.0 V, 186=103.5 V (semantics unclear, widened to 0–300); 77=0xFFFF (unset sentinel, entry disabled); 340 unread-yet (naming conflict open).
- Firmware: 542–547 reads zeros — NOT version registers. Candidate: reg 22 = 0x0203 (plausible "V2.03") — compare against LCD before adding.
- Grid/meter block 598–619 reads ~zero (open: CT/meter wiring or Meter Select setting).
- Topology: Pi offsite at 100.64.0.3; HA on tailnet; bridge binds 0.0.0.0:8300 with bearer token; plain ws:// (tailnet encrypts).

## Risks & open questions

1. **Sign conventions** (blocks Phase 3 trust, not Phase 1/2): grid 619 import-positive per Developer089 — unverified; battery 590/591 — sunsynk applies negative scale (native flip?) — unverified. Confirm under real flow; record in map comments when known.
2. **reg 340 naming conflict** (Max Sell vs Max Solar): read back, compare to LCD (~12 kW ⇒ sell, ~15.6 kW ⇒ solar).
3. **regs 172–177 encoding** (plain 0–3 select vs packed bitfields incl. mode bits 2–4, sell bit 5): read back vs LCD before any write; planner must encode correctly.
4. **reg 186 = 103.5 V**: likely not a grid low-voltage limit at all. LCD grid-protection screen decides; possibly remove the entry.
5. **Sentinel values** (0xFFFF = unset on 77): needs a schema concept (`sentinel:` field → Text("unset") or Option) before exposing such controls.
6. **tokio-modbus maturity** on Deye specifically: mitigate with mbpoll ground truth during bring-up (`probe` cross-check). Pin crates via Cargo.lock.
7. **WS keepalive over tailnet**: ping/pong ~30 s; integration reconnects on drop.
8. ~~**kameo_actors APIs unread**~~ — closed 2026-09-28: not used; plain mpsc subscriber fan-out (decision #8).
9. ~~HA on tailnet~~ — resolved. ~~Register addresses~~ — verified. ~~Word order~~ — verified low. ~~serde strictness~~ — solved via raw mirrors.
