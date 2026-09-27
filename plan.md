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

## Engineering gotchas discovered (do not re-learn these)

- **TOML integer keys don't deserialize into `BTreeMap<u16, _>`** ("invalid type: string, expected u16" — TOML keys are strings by spec). Options are therefore **arrays of `{ value, label }`** (`EnumOptions`). Same shape on the JSON meta.
- **serde `deny_unknown_fields` does NOT fire inside internally-tagged enum variants** (verified live: `base_word` on a u16 entry was silently dropped). Fixed via deny-strict raw mirrors (`RawEncoding`, `RawConstraint`) + `deserialize_with` conversions that enforce field combinations and reject cross-kind fields. Bonus: errors carry TOML span positions. If a new tagged enum is ever added to the schema, it needs the same treatment.
- **`#[serde(default)]` on `f64` yields 0.0, not 1.0** — scale uses `default = "unit"`.
- **crane's `cleanCargoSource` drops `maps/`** — `include_str!("../maps/...")` breaks under `nix build`. The flake `src` filter must be widened when the cross-build is wired up. (Local `cargo` builds are unaffected.)
- **Block reads:** sensors poll as 8 FC03 spans — (3) (20) (114) (210–220) (500) (514–541) (555–558) (586–679) — plus ~3 control read-back spans. The 586–679 span is 94 regs → **chunk at ≤64 regs per request**. Timing at 9600 8N1: ~1.15 ms/char, a 64-reg round-trip ≈ 200 ms incl. turnaround → ~11 blocks ≈ 2–3 s per cycle. Fits the 5 s interval; don't go below 5 s without re-measuring.

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
### Task 0.2: ~~Rust toolchain via nix on the Pi~~ → REPLACED: cross-compile on anriha-pc
No nix on the Pi. Instead:
- Add `aarch64-unknown-linux-musl` target (rust-overlay toolchain or cargo-zigbuild — decide at first build; both produce a static binary that runs on Pi OS trixie).
- **Widen the crane source filter** so `maps/` survives `nix build` (see gotchas). Flake currently declares `systems = [ "x86_64-linux" ]` only — fine for building the cross artifact on this machine.
- `justfile` recipes: `build` (debug/test), `build-arm` (aarch64 musl release), `deploy` (scp binary → Pi, restart service), `probe` (ssh + run probe), `test` (cargo test).
Expected: `just build-arm && just deploy` ships a running binary; `file` reports static PIE.

### Task 0.3: Stable serial device name on the Pi — OPEN (do at first deploy)
udev symlink `/dev/ttyRS485` for the CH340 dongle (vid 1a86 pid 7523 — verify with `udevadm info -a`). One-time ssh; config lives in the justfile/README, not nix.

---

## Phase 1 — Modbus communication working (Goal 1)

### Task 1.1: Wiring + link check — DONE 2026-09-27 (mbpoll reads OK, slave mode confirmed)
### Task 1.2: Smoke test identity — DONE via mbpoll; Rust `probe` must reproduce (below)
### Task 1.3: Register map file — **DONE**
`maps/sun_3ph_hybrid.toml`: 84 sensors, 46 controls. Every entry cross-checked against Developer089 + sunsynk and anchored to hardware captures (see comments inline). Notables:
- `base_word = "low"` on all 32-bit entries (hardware-verified twice).
- `serial_number` str/5 words @3; `rated_power` u32 @20; faults 555–558 raw; BMS mirror 210–220; battery-2 block included.
- Controls incl. TOU 148–177, SOC thresholds 115–117, current limits 108/109 (max 240 per live readback), grid protection 185/186 (widened, semantics unverified).
- Disabled: `active_power_regulation` @77 — reads 0xFFFF (unset sentinel; needs a `sentinel` schema concept before exposure).
- PV3/PV4 skipped (12K has 2 MPPTs).
### Task 1.3b: Typed loader + strict validation — **DONE**
`src/map.rs`: `RegisterMap`/`SensorDef`/`ControlDef`/`Encoding`/`Constraint`/`EnumOptions`, raw-mirror strict deserialization (see gotchas), `validate()` (unique names, options⇒u16, scale≠0, min<max, on≠off). `load_from_str` + `load(path)` + `EMBEDDED_MAP`. Tests: `tests/map_parse.rs` — **13/13 green**, incl. real reject-fixtures (TOML, not the old vacuous YAML ones) and corrected anchors.
### Task 1.4: Poller with decoding — **IN PROGRESS (decode body pending)**
- `src/decode.rs`: `Value` (Number/Text), `DecodeError`, `decode(&SensorDef, &[u16]) -> Result<Value>`. Contract: word-count check; options ⇒ enum-index lookup (no scale/offset); Str ⇒ big-endian byte pairs, trailing NULs trimmed, printable ASCII; numeric ⇒ sign per encoding, 32-bit assembled per `base_word` **before** sign extension, `value = (raw − offset) × scale`.
- `tests/decode.rs`: 11 vectors = the contract. Anchors: 0x1545→54.45 V; 0xFFF6→−10 (s16); u16 never sign-extended (0xFFFF→65535); temp (1165−1000)×0.1→16.5; **u32 base-low**: (534,535)=(0x022E,0x0000)→55.8 kWh and (20,21)=(0xD4C0,0x0001)→12 000 W; base-high mirror assembly-only; s32 sign across words; options 2→"normal", 7→UnknownOption(7); SN words→"2605096069"; word-count error.
- Block polling: group map addresses into contiguous FC03 spans, chunk ≤64 regs (see gotchas for spans/timing).
### Task 1.5: Robustness — OPEN
Reconnect with exponential backoff inside BusActor (drop + reopen serial; dongles hang after replug). `link_ok` in every snapshot; last-success timestamp. `config.toml`: port, baud, slave, interval, bind addr, token.
### Task 1.6: CLI — OPEN
`solar-bridge probe` (identity: read regs 3–7, assert SN 2605096069 — first cross-compiled run against real hardware), `top` (live decoded table), `dump` (one JSON snapshot to stdout). clap is already a dependency.
### Task 1.7: Actor skeleton — OPEN (new)
`src/bus.rs`: kameo `BusActor` — owns tokio-serial + tokio-modbus RTU client; interval poll; decode; `tell`s snapshot to StateActor. **No write handler in this phase** — the mailbox accepts reads' results only; the write message type arrives with Phase 3 so the planner is provably the first writer.
`src/state.rs`: `StateActor` — caches latest snapshot + link_ok; subscriber fan-out (plain Vec of ActorRefs or kameo_actors pubsub — read its API first).

---

## Phase 2 — Export to Home Assistant (Goal 2)

### Decision: Pi pushes state; HA sends commands (unchanged)
Rejected alternatives (for the record): HA RESTful polling (ownership inversion), Pi POSTs to /api/states (orphaned entities), MQTT discovery (user preference), ser2net + HA modbus (violates single-master), Matter (immature + wrong semantics). Chosen: one bidirectional WS, bridge pushes, entities native in HA.

### Task 2.1: Bridge WS server — OPEN
Per-connection kameo actors (`src/server.rs`; tokio-tungstenite directly or axum — decide when wiring; no TLS needed). Bind 0.0.0.0:8300, bearer token on upgrade (query param or header).
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
- HTTP `GET /state|/meta|/directives` for curl-debug. systemd user service on the Pi (`Restart=always`).
Expected: `websocat` sees hello+stream; a manual `cmd` flips a directive and persists across restart.

### Task 2.2: Custom HA integration — OPEN
`custom_components/solar_bridge/` (Python, aiohttp WS client, auto-reconnect, entities from `meta`; options arrive as `{value,label}` arrays). `link_ok` drives availability; one HA device; state_class per meta → Energy dashboard native. Install via `ssh hassio@192.168.1.187` or Samba addon.
### Task 2.3: Energy dashboard readiness — OPEN
Cumulative registers exposed `state_class: total_increasing` (already set in the map); wire solar/grid/battery in/out; sanity-check 24 h curves.
### Task 2.4: HA → Pi directives — OPEN (in scope per decision)
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
Cargo.toml  flake.nix  justfile (planned)  config.toml (planned)
maps/sun_3ph_hybrid.toml       # register map — DONE, 84 sensors / 46 controls
src/main.rs                    # CLI: probe | top | dump | serve (clap; minimal now)
src/lib.rs                     # pub mod decode, map (+ telemetry, bus, state, server, directives later)
src/map.rs                     # types + strict loader + validate — DONE
src/decode.rs                  # decode — IN PROGRESS (todo! body)
src/telemetry.rs               # planned: Snapshot, PlanningSignals, newtypes
src/bus.rs                     # planned: BusActor (sole serial owner)
src/state.rs                   # planned: StateActor
src/server.rs                  # planned: WS connection actors + HTTP debug
src/directives.rs              # planned: directive schema, validation, persistence
tests/decode.rs                # 11-vector contract — present
tests/map_parse.rs             # 13 strict-loading tests — green
systemd/solar-bridge.service   # planned (plain user service, no nix)
# on the HA box: /config/custom_components/solar_bridge/
```

## Validation summary

- Phase 0: `just build-arm` produces static aarch64 binary; `just deploy` runs it on the Pi.
- Phase 1: `cargo test` green (map 13/13, decode 11/11); `probe` prints SN 2605096069 from Rust; `top` matches LCD/SolarMAN; 24 h soak survives dongle replug without restart.
- Phase 2: HA entities stream live over WS (no polling); unavailable within a minute of bridge stop; Energy dashboard curves after 24 h; HA toggle → cmd → ack → directive persisted across restart.
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
8. **kameo_actors APIs unread** — verify from vendored source before use; plain ActorRef fan-out is the fallback.
9. ~~HA on tailnet~~ — resolved. ~~Register addresses~~ — verified. ~~Word order~~ — verified low. ~~serde strictness~~ — solved via raw mirrors.
