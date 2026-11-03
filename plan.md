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
5. **All entities on the existing Deye device** (same `{serial}` identifier) (superseded by #18 below, same session).
6. **HA write path:** `hub.async_cmd` — uuid id, pending-future map resolved by `cmd_ack` (ok=false → error surfaced in UI), pending failed on connection drop, 10 s ack timeout. Entity display always from server pushes — never optimistic. Directive entity availability gates on WS connectivity only (directives stay valid when link_ok=false); sensors still gate on link_ok.
7. **Division of work:** HA/Python side fully delegated to the agent (complete, in working copy); Rust changes scaffolded with DECIDE holes for the human: (a) DirectiveSpec representation + placement, (b) meta composition (typed envelope vs json merge), (c) controls decode (generalize decoder input vs second loop) + cross-kind name uniqueness in `validate()` (values is name-keyed; sensors∪controls dupes would silently overwrite) + `tests/decode_all.rs` coupling coverage for controls.
8. **Plan amended (D1): register writes pulled forward to now, HA as writer.** Was: "planner is provably the first writer" (2026-09-27 #5/Phase 3). Now: HA-writable control registers as a *debug tool* (rare manual writes, testing aid); planner directives stay but are secondary until Phase 3. Consequence: the future planner must treat registers as source of truth (read-before-write, no shadow state).
9. **Write protocol (D2): dedicated `write`/`write_ack` frames** — `{"type":"write","id","name","value"}` / `{"type":"write_ack","id","ok","error"?}`. Directives keep `cmd` untouched; DirectivesActor never sees register writes. BusActor's mailbox serializes writes with polls (single-master discipline preserved).
10. **Writable surface (D3): `writable = true` on ALL 64 active controls for now** — schema field is deny-by-default (`#[serde(default)]`), but the map sets true everywhere; 172–177/186 semantics still unresolved, so writes there are at the user's own risk until LCD-verified. Tightening later = flipping map entries.
11. **Validation (D4): out-of-range writes REJECTED, never clamped** — bridge is the authority; HA UI clamps are cosmetic.
12. **Readback (D5): FC06 → FC03 → raw equality before ok=true**; mismatch = error ack.
13. Open holes for the Rust fill: select-write value semantics (label vs u16 value), validation placement (server task vs BusActor), write-encoder placement + number representability policy — plus round-1 holes (directive_schema body, meta composition, controls decode, validate() cross-kind uniqueness, decode_all coverage).
14. **Write-path decisions (second batch):** select writes carry the option **u16 value** (1b — labels stay display-only, no new label-uniqueness invariant); **BusActor owns validation/encoding** (2b — ws reader stays thin, encoder lives bus-side as pure `encode_write`); **non-representable numbers rejected** (3a — 1e-6 float-artifact tolerance on `(value)/scale + offset`, genuine non-integers reject); **shared `Field` decode input** (4a — one decoder for SensorDef/ControlDef, controls' Select constraints feed the options path so enum readbacks decode to labels).
15. **Write path implemented:** `encode_write` (reject-never-clamp, s16 two's-complement wrap, writable gate, u16/s16-only) + `WriteRegister` message (FC06 with 2 s timeout → FC03 readback → raw equality; IO failure = record_failure + error ack; readback mismatch = loud error, link NOT flipped) + server routing via `AppState.bus` + `validate()` folds sensors∪controls into one name set. Tests: tests/write.rs (9: switch/select semantics, range reject, representability, offset roundtrip anchor, s16 wrap, gates, full real-map roundtrip), decode_all coupling covers controls, map_parse cross-kind duplicate.
16. Remaining: hardware bring-up verification of the write path (controlled window, LCD cross-check per plan gate), then `just deploy`, HA reload, tag `v0.0.2`.
17. **Independent review of the write path (pre-deploy):** fixed — controls `scale == 0` now rejected in `validate()` (NaN would silently write raw 0); protocol.rs contract comment updated to the settled select-u16 semantics; successful write+readback resets `fails`/`next_attempt` (polls no longer skip up to 60 s after a write proves the link); hub send-race OSError → BridgeCmdError; ControlEntity availability gates on `link_ok` too (reads AND writes are bus-dependent — unlike directives); ControlSwitch renders non-{on,off} raws as unknown; `_state_from` directives param required (dead default branch). **Open for the hardware window:** FC03 readback fires immediately after FC06 — Deye may serve a stale register on an eager readback; if spurious Readback errors show up, add a 50–200 ms settle delay (measure first, don't guess). Full suite 68/68 green.
18. **HA devices: planner separated** (supersedes #5): everything stays on the Deye inverter device exactly as before — readback sensors AND control write widgets (HA shows them as the Sensors/Controls sections of the one device page, which is the desired picture). Only the four directive entities move to a new "Solar Bridge planner" device (`{serial}-planner`, `via_device` the inverter) — planner override state is conceptually not inverter telemetry. A three-way split (writes on their own device) was briefly applied and reverted same session: write widgets belong next to their readbacks. `unique_id`s unchanged — entities regroup on integration reload, nothing orphans.

## Decisions log (2026-11-04 session)

1. **A lagging subscriber is never dropped — supersedes 2026-09-28 #8 ("lagging = pruned on `try_send` failure").** `StateActor::broadcast` now skips a frame for a connection whose queue is full and keeps the connection; only a *closed* queue is reaped. Every push is a complete snapshot, so a skipped frame self-heals on the next tick. Reaping on `Full` cost HA 19 h 30 m on 2026-11-03: the writer task ended, the WS socket stayed ESTABLISHED, axum kept answering protocol pings, and the client's read loop never received another frame — no exception, no `unavailable`, no reconnect. Pinned by `tests/subscribers.rs::full_channel_does_not_unsubscribe`, which was verified red against the pre-fix broadcast.
2. **Liveness is proven on the message channel, not by TCP.** `hub.py` pings `{"type":"ping"}` every 30 s over the WS — answered by the bridge's own connection task, the same one that pushes — and reconnects when no frame of any kind arrives for 90 s (`APP_PING_INTERVAL` / `SILENCE_TIMEOUT`). Protocol-level pings cannot see this failure: axum answers them below the feed.
3. **A peer that blocks a write for > 180 s is dropped** (`SEND_TIMEOUT`, `server.rs`). The writer ending now closes the socket (the reader `select!`s on the writer handle) instead of leaving a peer that looks connected and is deaf.
4. **The client closes the old socket before reconnecting.** Abandoned connections are what left two dead half-sockets on the bridge (one since 2026-10-29, one since 2026-10-31; the older had absorbed 262 MB of pushes against 307 bytes of client traffic).
5. **Logging stays `eprintln!` with `[tag]` prefixes** (`[state]`, `[server]`, `[bus]`, `[main]`) — no `tracing` dependency: journald already timestamps, the crate stays light for the static musl build, and the existing `[bus]` lines set the convention. Transition lines only (behind / caught up / reaped / link up), never one per frame.
6. **Enum readback sensors carry `device_class: enum`.** `sensor.py` set `_attr_options` without it, so `run_state`, `work_mode`, `gen_port_usage` and `tou_charge_source_1..6` failed to add on every setup ("providing enum options, but is missing the enum device class") and sat `unavailable` since 2026-10-29.
7. **The Pi's journal is made persistent.** `Storage=auto` is already set, but `/var/log/journal/<machine-id>` never existed and `konazumi` is not in `systemd-journal`, so the incident left no Pi-side trace at all. Recipe in `systemd/README.md`.
8. **Map counts corrected in the plan.** Was: 84 sensors / 46 controls in the Files block and
   90/64 in Task 1.3; now **90 sensors / 65 controls**, measured from the parsed TOML (all 90
   sensors enabled, all 65 controls `writable = true`, none disabled). The control count grew with
   `tou_enabled` (2026-11-04).
9. **Efficiency model = the four measured paths, PV on the DC side** (supersedes "three
   directional curves" in Phase 3a item 7). `pv_to_battery` 94.7 / 94.3 / 92.8 / **87.0** % across
   5–9 / 3–5 / 1.5–3 / <1.5 kW, `pv_to_grid` 96–98 %, `battery_to_grid` 96.9 / 96.0 / 93.9 %
   across 6–11 / 3–6 / <3 kW, `grid_to_battery` ~95 % (**unmeasured** — the AC charger leg), plus
   `idle_kw` ≈ 0.09 kW charged to whichever source is active in the MTU. `Slot::pv_kw` is **DC**;
   every AC-side quantity is produced by one of the curves. There is no fifth round-trip stage:
   `0.947 × 0.969 = 0.918` sits inside the measured daily terminal ratio 0.918–0.932, so that ratio
   is a consistency check on the curve values, not a number in the model. **PV-first is a
   dominance rule, not a tuning preference:** the direct route crosses one curve, the same kWh
   cycled crosses two, and no state makes the detour cheaper.

## Decisions log (2026-11-06 session)

1. **The plan is its own frame — one per change, not one per poll.** It rode every 5 s
   telemetry frame: a full horizon is 240 MTUs ≈ **78 KB**, i.e. **1.2 GB/day per subscriber**,
   for a value republished once a minute. `ServerMsg::State` now carries telemetry only,
   `ServerMsg::Plan { plan: Option<Plan> }` goes out when the planner publishes (and with an
   explicit `null` on withdrawal), and `Hello` / `/state` still carry the complete snapshot.
   Because the plan is then the one message a skipped frame could lose, a connection that
   catches up is handed the current plan again — once, at the `lagging` → `live` transition
   (`StateActor::push`). Pinned by `tests/subscribers.rs` (separate frame, explicit null,
   replay on catch-up).
2. **The plan reports metered AC, not the commanded power.** `PlanStep::ac_kwh` is signed
   `ac_out − ac_in`. `action.kw` is a target the plant may not reach (12 kW import against a
   10.24 kW DC pack limit bills ≈2.72 kWh; a PV-less export tops out at ≈9.9 kW AC), so the
   calendar's event summary keeps the command while its description shows the meter.
   `net / |ac_kwh|` is then the price actually achieved in both directions.

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
`maps/sun_3ph_hybrid.toml`: 90 sensors, 65 controls. Every entry cross-checked against Developer089 + sunsynk and anchored to hardware captures (see comments inline). Notables:
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
- Before ANY first write: controlled window, one register at a time, readback-verified, LCD cross-checked. Resolve the 172–177 bitfield-vs-select question first (see Risks).

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
maps/sun_3ph_hybrid.toml       # register map — DONE, 90 sensors / 65 controls
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
src/units.rs                   # DONE — AC/DC/time/money newtypes, one curve per path
src/planner.rs                 # DONE — DP solve, Action/Reason/Plan/PlanStep, ValueTable
src/planner/{price,forecast,horizon,actor,actuator}.rs  # DONE — OTE + forecast.solar, horizon, cycle, plan->TOU
custom_components/solar_bridge/plan_view.py   # DONE — plan -> runs + wording, no HA imports
custom_components/solar_bridge/calendar.py    # DONE — one calendar event per run
tests/planner.rs  tests/plan_wire.rs          # DONE — planner through its actors, wire contract
tests/test_plan_view.py                       # DONE — 13 stdlib tests, no HA install needed
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
- Control read-backs: 108=109=200 A (device default → constraint max 240); 185=276.0 V, 186=103.5 V (semantics unclear, widened to 0–300); 77=0xFFFF (unset sentinel, entry disabled); 340 = 13200 W, 143 = 12000 W (naming resolved 2026-10-11: 340 Max Solar, 143 Max Sell).
- Firmware: 542–547 reads zeros — NOT version registers. Candidate: reg 22 = 0x0203 (plausible "V2.03") — compare against LCD before adding.
- Grid/meter block 598–619 reads ~zero (open: CT/meter wiring or Meter Select setting).
- Topology: Pi offsite at 100.64.0.3; HA on tailnet; bridge binds 0.0.0.0:8300 with bearer token; plain ws:// (tailnet encrypts).

## Risks & open questions

1. **Sign conventions** (blocks Phase 3 trust, not Phase 1/2): grid 619 import-positive per Developer089 — unverified; battery 590/591 — sunsynk applies negative scale (native flip?) — unverified. Confirm under real flow; record in map comments when known.
2. ~~**reg 340 naming conflict** (Max Sell vs Max Solar)~~ - resolved 2026-10-11: LCD + upstreams confirm 340 = Max Solar Power (renamed max_solar_power); 143 = Max Sell Power (renamed from max_grid_output_power).
3. **regs 172–177 encoding** (plain 0–3 select vs packed bitfields incl. mode bits 2–4, sell bit 5): read back vs LCD before any write; planner must encode correctly.
4. **reg 186 = 103.5 V**: likely not a grid low-voltage limit at all. LCD grid-protection screen decides; possibly remove the entry.
5. **Sentinel values** (0xFFFF = unset on 77): needs a schema concept (`sentinel:` field → Text("unset") or Option) before exposing such controls.
6. **tokio-modbus maturity** on Deye specifically: mitigate with mbpoll ground truth during bring-up (`probe` cross-check). Pin crates via Cargo.lock.
7. **WS keepalive over tailnet**: ping/pong ~30 s; integration reconnects on drop.
8. ~~**kameo_actors APIs unread**~~ — closed 2026-09-28: not used; plain mpsc subscriber fan-out (decision #8).
9. ~~HA on tailnet~~ — resolved. ~~Register addresses~~ — verified. ~~Word order~~ — verified low. ~~serde strictness~~ — solved via raw mirrors.
10. ~~**Battery DC power limit is not modelled**~~ — **resolved 2026-11-06.** `Params::battery_max_kw`
    (10.24 kW DC) bounds the pack's share in both directions inside `next_energy`
    (`from_pack` for discharge, `room_left` for charge, the latter shared with the PV charge
    leg), and `pack_power_limit_bounds_both_directions` pins that a 12 kW command delivers
    only 10.24 kW DC and, at η=0.9, 9.216 kW AC at the meter.

## Phase 3a — Planner decisions (2026-11-03)

Scope for now: **selling only** — EDC sharing to the home is not wired yet (~1 month). Grid
charging is in the model as an action (`D1b`, "available and simple to make"), gated by price;
selling is always allowed, buying is the careful side.

**Amended from the 2026-09-27 sketch:** "planner is provably the first writer" is long gone — HA
writes control registers today, and the sell path is proven on hardware (grid export counters
moved 14.8 / 16.3 / 16.8 kWh on Oct 31–Nov 2). The planner goes live immediately; no soak gate.

### Settled

1. **Prices** — OTE 15-min day-ahead, **EUR/MWh natively**; CZK is a ČNB daily-rate conversion the
   HA integration does today and the Pi must do itself. Tomorrow's prices appear **~13:10 Prague**;
   before that the same request simply omits tomorrow, so the horizon ends at the last published
   MTU. **The Pi fetches everything itself — no HA dependency in the planner path.** Primary:
   OTE's public SOAP service (`POST /services/PublicDataService`, `pub:GetDamPricePeriodE`,
   `PeriodResolution=PT15M`, no key); fallback: per-day XLSX
   (`/pubweb/attachments/01/{Y}/month{MM}/day{DD}/DT_15MIN_{DD}_{MM}_{YYYY}_CZ.xlsx`), or ENTSO-E
   A44 once a token exists. Handle Prague civil dates + `PeriodIndex`, never assume 96 MTU/day.
   **Verified 2026-11-03:** `GetDamPricePeriodE` takes `StartDate`/`EndDate` as `xs:date`, so a
   historical window is *one* request (27 Oct – 2 Nov returned 288 items in 70 ms, no nulls) — the
   7-day lookback needs no XLSX parser and no local price store. `Price` is `minOccurs="0"`, so
   skip null items rather than reading them as 0. ČNB `api.cnb.cz/cnbapi/exrates/daily` *ignores*
   `?currency=EUR` and returns all 30 rows: filter `currencyCode == "EUR"` and divide `rate` by
   `amount` (24.34 CZK/EUR on 2026-11-03).
2. **Tariff** — sell = `spot − 0.50` CZK/kWh; buy = flat **5.00 CZK/kWh** (single-rate household,
   all-in) as a tunable placeholder.
3. **Method** — **dynamic programming** over a discretized battery-energy grid (`V_t(E)`,
   forward argmax walk, marginal value by finite difference = the explanation). Chosen because the
   actuator is window+setpoint shaped, `η(P)` must sit inside the objective (slow-drain vs burst),
   and it needs no solver crate. Rejected: LP + Clarabel (continuous power cannot express the
   window actuator; η(P) would need a concave piecewise approximation); MILP + microlp (no duals,
   so no "why") and HiGHS (C++/CMake/bindgen; static musl cross-build broken since 2023). LP/MILP
   become viable only if the actuator gains true continuous power control.
4. **Actuator** — `work_mode` gate (`selling_first` + `tou_enabled=off` to sell,
   `zero_export_load` + `tou_enabled=on` to hold) **plus a per-MTU `max_sell_power` setpoint**
   (reg 143, 0–12000 W) so the DP can choose slow vs burst. **Amended 2026-11-05: the export
   cap is trusted, not a setpoint.** Reg 619 (external CT) still reads ~0 and the CT lands within
   days, but reg 607 reports grid import/export correctly and the grid counters track, so
   Max Sell Power is expected to work — the planner uses the commanded power as a limit, and any
   PV the pack cannot absorb and the cap cannot export is curtailed.
5. **Forecast** — `api.forecast.solar`, the same single plane HA is configured with: **16.5 kWp,
   azimuth 200°, tilt 37°**, hourly `wh_period` spread over its four quarters, with a
   measured/forecast calibration ratio. Measured optimism: 2026-11-03 forecast 18.86 kWh vs actual
   8.2 kWh (2.3×) — the ratio is not optional. Separate module so the model can be replaced.
6. **Battery** — `capacity_kwh = 32.0` (reg **102 = 628 Ah** at ~51.2 V). Reg **592 reads 200**
   and is NOT the pack; the map comment calling it "BMS-corrected" is wrong (fix mechanically).
7. **Config/tuning split** — boot-only settings stay in TOML (port, baud, token, endpoints, array
   geometry); everything tunable lives in **`Directives`** (persisted, validated, reject-never-clamp
   like the write path). `soc_target_hold` and `price_override` are **deleted**; `planner_enabled`
   and `force_mode` stay. New fields: `soc_floor 0.20`, `soc_ceiling 0.80`, `max_power_kw 12`,
   `buy_price_czk 5.0`, `sell_fee_czk 0.5`, `min_sell_price 0`, `forecast_calibration 1.0`,
   `battery_charge`/`battery_discharge`/`inverter` efficiency curves (points),
   `energy_step_kwh 0.32`, `action_step_kw 1.2`.
   Prices are never set from HA — the authoritative source is the Pi's own feed.
   **Amended** (was: `export_max_kw 12` / `import_max_kw 12` / `charge_max_kw 12` /
   `discharge_max_kw 12`; now: one `max_power_kw 12` plus three directional efficiency curves,
   because the four caps were never independent — the inverter bounds export, import, charge and
   discharge alike, and the directional difference belongs in `eta(P)`, not in a second cap).
   **Amended again 2026-11-04: four measured path curves, not three** (see decision 9 below).
   Also `action_step_kw` 1.0 → 1.2, because 1.2 / 6.0 / 12.0 land exactly on the 1.2 kW grid.
8. **Where it runs** — one binary: `PlannerActor` inside `solar-bridge`, publishing the plan
   through `StateActor` (visible in HA) and writing through `BusActor`.

### Layout (Rust module root + submodules)

```
src/planner.rs            units, Mtu/Slot/Params/Problem, Action/ActionLadder/PlanStep/Plan/Reason,
                          EfficiencyCurve, TerminalValue, ValueTable, solve/next_energy — DONE
                          (1381 lines + 19 tests; the user's own draft, graded 2026-11-05)
src/planner/price.rs      OTE SOAP client (index → prices), cache, publication edge — DONE
src/planner/forecast.rs   forecast.solar client, hourly→quarter spread — DONE (calibration is
                          horizon policy, not a client concern)
src/planner/horizon.rs    prices + PV + load → Vec<Slot>, HorizonActor (own refresh) — DONE
src/planner/actor.rs      PlannerActor: 60 s tick, solve, publish via StateActor — DONE
src/planner/actuator.rs   Plan → writes (mode gate + max_sell_power), drift reconciliation — TODO
```

### Settled in the scaffold (the former `DECIDE:` blocks are closed)

(a) `Action` stays `Idle | Export{kw} | Import{kw}` — no constrained struct.
(b) Ladder = the uniform `action_step_kw` grid **union** the three efficiency knots, sorted and
    deduped. At 1.2 kW that is 11 points, with the knots landing on steps 1/5/10. The union is the
    contract: a future knot change costs one extra action, never a mispriced kWh.
(c) `TerminalValue` is a single number: median over the 7 days ending *today* of each day's max
    spot price, minus the sell fee. Today, not yesterday, because a day-ahead curve is complete
    before the day begins — including it adds the freshest data without any intraday drift.
    Refreshed on the daily price fetch, not per solve; a stale value for a few hours is harmless
    because T is only a level and never enters a transition. Measured 2026-11-03: **8.474 CZK/kWh
    spot → 7.974 net**, against 8.342 (mean of daily maxima) and 5.583 (median of all MTUs); the
    13.43 CZK/kWh spike on 29 Oct is correctly ignored. Buying against T is **allowed** — after
    13:10 the horizon carries tomorrow's real prices and dominates the tail estimate anyway.

### Scaffold shapes closed (2026-11-04, second session)

1. **Ladder data:** `ActionLadder::new(&Params)` builds two stored vectors (`export`, `import`),
   each the union of the `action_step_kw` grid and the three curves' knots — sorted, deduped,
   0.0 present, all ≤ `max_power_kw`. Equal today (one `max_power_kw` bounds all four directions);
   carried separately so a directional rating needs no new type.
2. **`next_energy` returns `Flow { energy_after_kwh, battery_in_kwh, battery_out_kwh }`**, not a
   tuple — the accepted-charge side is what `Reason::PvCannotBeStored { headroom_kw }` needs, and a
   three-element tuple would be unreadable at the call site. Unclamped; `solve` clamps to the grid.
3. **`TerminalValue` is a struct, not an enum.** `Zero` and `Floor { soc }` deleted: a reserve
   belongs to `soc_floor` policy, not to the objective, and a zero terminal dumps the pack on the
   last MTU.
4. **Horizon construction belongs to `planner/price.rs`.** `planner.rs` stays a pure function of
   already-built `slots`; `horizon_edge_is_the_last_published_mtu` and
   `dst_day_does_not_assume_ninety_six_mtus` move there when that module is scaffolded and are
   parked in `planner.rs` until then.

### Scaffold state (2026-11-04 19:00)

`pub mod planner;` is in `src/lib.rs`. `src/planner.rs` is **433 lines, 22 `todo!()`, 16 named
tests**; `cargo check` clean (13 dead-code warnings, expected — nothing constructs these types
yet), `cargo test` 0/16 on the lib target (all "not yet implemented") and **75/75 green** on the
integration targets. Implemented: `EfficiencyCurve::new`/`flat`/`at` and the two ladder accessors.
Awaiting the user's draft: `ActionLadder::new`, `next_energy`, `build_value_table`,
`ValueTable::at`/`marginal`, `solve`, and all 16 test bodies.

Not created: `planner/price.rs`, `forecast.rs`, `actuator.rs`, `actor.rs`. The price module's API
(OTE fetch, ČNB conversion, publication edge, horizon assembly) is the next decide batch after the
DP core is drafted.

### Phase 3a state (2026-11-06, third session)

`cargo test` **124 passed / 0 failed** (1 ignored live-OTE check); `nix build .#solar-bridge-arm`
exit 0 with `reqwest` at its **default** features — `aws-lc-rs` cross-compiles for aarch64-musl
under crane, so the planned `rustls-no-provider`/ring workaround was unnecessary.

What exists now:

- `price.rs` — `OteClient` (two SOAP POSTs, index first), pure parsers pinned against captured
  fixtures in `tests/fixtures/`, `PriceCache` with `covers`/`age_secs`. No ČNB client: `EurRate`
  comes from OTE's own index, including for the delivery day.
- `forecast.rs` — `ForecastSolar` with site geometry; hourly `watt_hours_period` → four MTUs;
  keys are local wall-clock strings; azimuth is sent compass − 180.
- `horizon.rs` — `assemble` joins on `Mtu::start` (priced-without-PV = clear sky; forecast on an
  unpriced MTU *inside* the horizon = error; forecast beyond it is ignored), and `HorizonActor`
  refreshes independently — `build` never fetches, `NoPrices` is the boot state.
- `actor.rs` — `PlannerActor::cycle`: directives → horizon params → solve → publish, on a 60 s
  driver. Device constants live here (32 kWh, 10.24 kW DC, four measured curves); policy lives
  in `Directives`.
- Publish path: `StatePayload::plan` rides every frame and `/state`; `Update::Plan(None)`
  withdraws it when the planner is disabled. (**Superseded 2026-11-06** — the plan is its own
  frame; see the decisions log and the fourth-session state.)
- `Directives` — the Phase 3a tuning surface (soc_floor/ceiling with cross-field validation,
  max_power_kw, buy_price_czk, sell_fee_czk, forecast_calibration, energy_step_kwh,
  action_step_kw). `soc_target_hold` and `price_override` are deleted.
- `telemetry.rs` — `PlanningSignals` (battery_soc, battery_power), loud on missing/renamed, with
  a coupling test through the real map.

Open at the end of that session (the HA surface was finished in the fourth, below):

- `planner/actuator.rs` — drafted here and **wired into `main.rs` afterwards**: a deploy of the
  current tree writes registers, it is not a read-only deploy. No enable switch exists yet.
- `TerminalValue` is the constant 7.974 CZK/kWh; the daily median-of-daily-maxima refresh job and
  its 7-day price window are unwritten.
- ~~No HA entity for the plan (it is in `/state` and on every push only)~~ — see the fourth session.
- Nothing deployed since 2026-11-04; the Pi still runs the pre-planner binary.

### Phase 3a state (2026-11-06, fourth session) — the plan is a Home Assistant entity

`cargo test` **135 passed / 0 failed / 1 ignored** (live-OTE check); `python3 -m unittest`
**13 passed**. `just test` runs both suites now.

The plan was readable only in `/state` and on every push. It now has a surface on the HA side:

- `plan_view.py` — deliberately **no HA imports**, so `tests/test_plan_view.py` exercises it
  without an HA install. `segments()` merges consecutive MTUs whose action and power are equal
  into a run and **sums** the run's money, metered energy and discharged kWh — the figures
  describe the whole run, not its last quarter hour (which is what the first draft showed).
  `at()` / `upcoming()` answer "what now / what next"; `by_day()` rolls the runs up per local day.
- `calendar.py` — `calendar.<serial>_plan`, one event per run: summary `Sell 10.8 kW` /
  `Buy 12.0 kW` / `Hold`, description `1 h 30 min · 5.4 kWh sold · 5.48 CZK/kWh · net +28.60 CZK
  · pack 20.5 kWh · price above stored value`. Attributes: `expected_net_czk`, `horizon_end`,
  `computed_at`, `segments`. Derived from the live push state — a stored copy would only go stale.
- `sensor.py` — `Plan expected net` (CZK, `MONETARY`): the horizon total as the state, with
  `days` (sell/buy kWh and net per local day), `next`, `horizon_end` and `computed_at` as
  attributes. The calendar carries the actions; this carries the money, which only exists once
  the quarter hours are summed.
- `PlanStep::ac_kwh` (**wire change**, decision 2) — metered AC for the MTU, signed
  (`ac_out − ac_in`).
  `action.kw` is a *target*, not what the meter sees: an import is billed only for what the
  charger and the load actually took (12 kW offered against a 10.24 kW DC pack limit bills
  ≈2.72 kWh, not 3.0), and an export with no PV tops out at `battery_to_grid(10.24 kW)` ≈9.9 kW
  AC. With the signed field, `net / |ac_kwh|` is the true price in both directions. Pinned by
  `tests/plan_wire.rs`; `plan_view` falls back to `kw × hours` for a bridge that predates it.

Decision — **the power shown is the command, the energy shown is the meter.** The event summary
repeats `action.kw` because that is what is written to `max_sell_power`; the description's kWh is
`ac_kwh`, so the two can always be checked against each other.

Open:

- ~~**Deploying the current tree also enables control.**~~ **CLOSED 2026-11-06 (fifth session)** —
  `actuator_enabled` landed, default off; a deploy is report-only until HA flips it.
- ~~`TerminalValue` is still the 7.974 CZK/kWh constant.~~ **CLOSED 2026-11-06 (fifth session)** —
  measured on each price refresh; the constant survives only as the pre-history bootstrap.
- Nothing deployed since 2026-11-04; the Pi runs the pre-planner binary, so the calendar and the
  sensor exist in the working copy only until `just deploy` and a HA reload.

### Phase 3a state (2026-11-06, fifth session) — the control switch, a measured terminal value, and the DP handed back

`cargo test` **144 passed / 0 failed / 2 ignored** (both live-OTE), `python3 -m unittest`
**13 passed**, clippy clean.

- **`actuator_enabled`** — bool directive, **default off**, `#[serde(default)]` so a
  `directives.json` from the deployed pre-switch binary loads (off) instead of failing startup.
  One predicate, `writes_allowed(&Directives, link_ok)` in `planner/actor.rs`, gates the whole
  apply path; `planner_enabled` still decides whether a plan *exists*. The HA switch appears from
  `meta.directives` with no Python change. A disabled actuator still solves and publishes — the
  guard skips the write, never the cycle.
- **`TerminalValue::measured(points, now, sell_fee)`** — the rule settled on 2026-11-04 (median
  of each day's maximum over the 7 Prague days ending today, minus the fee), as code.
  `HorizonActor::refresh` now fetches `today − 6 .. today + 2` (`PRICE_HISTORY_DAYS`, one less
  than `TERMINAL_DAYS` because the window includes today) and caches the measured value from the
  same fetch; `build` ignores the history it brings back. `PlannerActor` uses
  `horizon.terminal()` and falls back to `bootstrap_terminal()` (7.974) only before the first
  refresh. Live check `live_ote_serves_a_week_of_history`: OTE serves the widened window and
  measures **7.984 CZK/kWh** on 2026-11-04 — the bootstrap constant is off by 0.01, so the rule
  and the measurement agree.
- Known shape of the failure: if the widened fetch were ever refused, the terminal value would
  silently stay bootstrap. The only tripwire is that ignored live test, run by hand.

**The DP core, authored by him (2026-11-06, same session).** `build_value_table` and `solve`
were reverted to a scaffold and written by the user, block by block, with the contract doc as
the spec: the backward pass over the 61-level grid (`V_t(E) = max_a money_t(a) + V_{t+1}(E')`,
the "fell AND below the floor" rejection, `clamp` before the interpolated lookup, `None` → the
loud `NoFeasibleAction`), then the forward walk (`table.at(index + 1, after)`, strict-improvement
tie-break so `Idle` wins, marginal taken before the step, `battery_out_kwh` floored, `explain`
given only the future slots). Both drafts needed one correction each — a missing
`rows[index] = row;` and nothing at all in the walk.

Everything is un-parked: `just test` → **144 Rust passed / 0 failed / 2 ignored** (the live OTE
checks) and **13 Python passed**. `nix build .#solar-bridge-arm` exit 0 on the final tree.

Still not done: **nothing is committed** (36 files in the working copy, no description) and
**nothing is deployed** — the Pi still runs the pre-planner binary. When it is deployed,
`actuator_enabled` is off, so it plans and reports (calendar + `Plan expected net`) without
touching a register until the switch is flipped in HA; that first switch-on is the moment to
use a controlled window, per the Phase 3 write protocol.

### Phase 3a state (2026-11-06, sixth session) — actuator safety, device routing, charger limit

`cargo test` **153 passed / 0 failed / 2 ignored** (the live-OTE checks), `python3 -m unittest`
**13 passed**, no new clippy warnings, `nix build .#solar-bridge-arm` exit 0. Working copy only;
nothing deployed.

- **Disarm is now a state, not an absent write.** `Directives::disarm` (bool, default off, a
  switch in HA) writes the safe state and holds it: `zero_export_load`, `max_sell_power 0`,
  `solar_sell` off, grid charge off, all six slots at SOC 100 / power 0 / charge off. The
  same state is applied when `planner_enabled` or `actuator_enabled` is switched off (on the
  transition), when no plan has been produced for `DISARM_AFTER_SECS` (600 s), on any refused
  write, and on SIGTERM/ctrl-c. Until now switching the planner off left the last schedule
  armed on the inverter.
- **`force_mode` deleted.** It was an HA select and a persisted field read by nothing. `load()`
  drops the retired key so a pre-deletion `directives.json` still loads instead of failing
  `deny_unknown_fields`.
- **PV routing corrected (B4).** `next_energy` now follows the unit: load → battery → grid.
  PV charges the pack before it reaches the meter, and only DC the pack refuses goes direct
  (`pv_to_grid`); an export target is met by the pack and the PV spill. A sunny sell now pays
  the round trip — the synthetic 24 h probe drops `expected_net` 275.2 → 260.1 CZK — and this
  is the first thing to verify on a live sunny sell.
- **Grid charger limit.** `Params::grid_charge_max_kw` = 185 A × 51.2 V = 9.472 kW; the actuator
  clamps reg 128 to 185 A and logs when the plan asked for more. The old 200 A clamp was above
  the map's own 185 A maximum, so every import ≥ 9.6 kW was rejected by the bridge — and the
  failure path had already enabled grid charge at the previous current.
- **Measured energy is no longer clamped into the band.** The value grid gains the measured
  level when it sits below the floor or above the ceiling, and the walk starts there. The floor
  is an **export** bound: an action that would export below it is skipped, while a discharge
  that only serves the ~90 W site load is allowed.
- **The first MTU is scaled** to the time left in it (`Problem::now`), so a plan computed at
  10:07 no longer counts 15 minutes of a 10:00 MTU.
- **Segment projection stops at local midnight** and uses `mtu.hours` instead of a hardcoded
  900 s. A target missing from the observed snapshot is an error in `writes_for`, not a blind
  write every tick.
- **Write order:** slots → charge current → charge enable → max sell → solar sell → work mode.
  A hold/disarm moves the mode to the front; arming selling is the last write.

Still open: 172–177 encoding (tested by the user, not LCD-cross-checked in this tree),
`max_sell_power = 0` semantics, `tou_power` as the export target rather than the pack's share
(monitoring — expected to go PV-first), `marginal`/`explain` units, a cycle cost, and the first
live write.
