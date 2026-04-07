# Changelog

## [0.3.1] - 2026-04-07

### Fixed

- **Sticky mux reconnect.** Once the bridge connects via mux, all subsequent
  reconnects use mux-only mode (`connect_mux_auto`). Previously, each reconnect
  re-ran the full mux→USB fallback chain, which could steal the serial port
  from the mux during a brief disconnect.

## [0.3.0] - 2026-04-07

### Changed

- Upgraded to `donglora-client` 0.2: all connections now auto-validate via
  ping-on-connect, rejecting non-DongLoRa serial devices within 200ms.
- Simplified connection logic — removed manual mux/serial fallback and manual
  ping; `donglora_client::connect()` handles the full fallback chain and
  validation internally.
- Config negotiation detects mux vs serial via transport type instead of
  string comparison.

### Fixed

- Repository and homepage URLs now point to `github.com/donglora/bridge`
  (was `swaits/donglora-bridge`).
- README links updated to `donglora` GitHub org.

## [0.2.2] - 2026-04-06

### Fixed

- TUI now fits in 80-column terminals (was 86).

## [0.2.1] - 2026-04-06

### Fixed

- CI: removed stale sibling-clone step, added `libudev-dev` dependency,
  upgraded `actions/checkout` to v5 (Node.js 24).

## [0.2.0] - 2026-04-06

### Added

- Auto-detect connection mode: when no `port` is configured, the bridge now
  tries the mux daemon first and falls back to direct USB serial
  (`find_port()`) if the mux is unavailable. Previously it would retry the mux
  forever.

### Changed

- Config negotiation on direct serial connections now pushes `SetConfig` with
  the desired radio config when it differs from the dongle's current config.
  On mux connections, the existing behavior (accept the mux's config) is
  unchanged.

## [0.1.0] - 2026-04-06

Initial release.
