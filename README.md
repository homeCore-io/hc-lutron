# hc-lutron

[![CI](https://github.com/homeCore-io/hc-lutron/actions/workflows/ci.yml/badge.svg)](https://github.com/homeCore-io/hc-lutron/actions/workflows/ci.yml) [![Release](https://github.com/homeCore-io/hc-lutron/actions/workflows/release.yml/badge.svg)](https://github.com/homeCore-io/hc-lutron/actions/workflows/release.yml) [![Dashboard](https://img.shields.io/badge/builds-dashboard-blue?style=flat-square)](https://homecore.io/lf-workflow-dash/)

Bridges Lutron RadioRA2 devices into HomeCore via the Lutron Integration Protocol (LIP) over telnet.

## Supported device types

| Kind | HomeCore device_type | Notes |
|---|---|---|
| `dimmer` | `light` | Brightness 0-100, configurable fade time |
| `switch` | `switch` | On/off relay |
| `keypad` | `button` | Press/release/hold/double-click events, LED state read/write |
| `pico` | `button` | Button events (read-only, no LEDs) |
| `occupancy_group` | `occupancy_sensor` | Occupied/vacant |
| `vcrx` | `button` | VCRX receiver with button outputs and CCI contact closure inputs |

## Scenes (phantom buttons)

`[[scenes]]` entries map phantom buttons on the Main Repeater to HomeCore devices. Send `{"activate": true}` to trigger a scene. LED state is tracked automatically (+100 offset from button component).

## Setup

1. Copy `config/config.toml.example` to `config/config.toml`
2. Set the repeater IP and integration credentials
3. Add device entries with integration IDs from RadioStar or `http://{repeater_ip}/DbXmlInfo.xml`
4. Add a `[[plugins]]` entry in `homecore.toml`

## Configuration highlights

- `default_fade_secs` — global fade time (per-device override with `fade_secs`)
- `hold_threshold_ms` — how long a button must be held before a "hold" event fires
- `[[scenes]]` — phantom button mappings with `main_repeater_id` and `button_component`
