# solar_controller

RS485 bridge for a Deye SUN-12K-SG05LP3-EU-SM2 hybrid inverter: polls the register map, publishes
it to Home Assistant, plans the next ~35 hours against day-ahead spot prices and a PV forecast,
and drives the inverter through its own TOU registers.

The binary is the whole service. It renders nothing — history is in Grafana, the horizon is a
JSON document.

## Surfaces

| endpoint | who reads it |
|---|---|
| `ws://…/ws?token=…` | Home Assistant (`hello`, `state`, `plan`, `cmd`, `write`, `ping`) |
| `GET /metrics` | Prometheus text exposition — odometer counters, power/SOC gauges, the prices the plan is priced with, the PV belief, the plan's command, link quality |
| `GET /plan.json` | the horizon as flat JSON (Grafana's Infinity datasource); `from`/`to`/`history` params |
| `GET /state`, `/meta`, `/directives` | debugging |

`/metrics` and `/plan.json` are unauthenticated like `/state`; only `/ws` is token-guarded, because
it is the one interface that can move an actuator.

## Layout

```
src/            the bridge (bus, decode, planner, state, server, metrics)
maps/           the register map — the single contract artifact
custom_components/solar_bridge/   the Home Assistant integration
systemd/        the user unit for the Pi
tests/          integration tests + the pinned /plan.json fixture
plan.md         the engineering journal: decisions, measurements, gotchas
```

## Build, test, deploy

```sh
just test        # cargo test + python unittest
just build-arm   # aarch64-musl static binary
just deploy      # scp to the Pi and restart the unit
```

Monitoring — the Prometheus scrape, recording rules and Grafana dashboards — lives with the rest
of the fleet's configuration in `~/.dotfiles/nina/solar.nix`, not here.
