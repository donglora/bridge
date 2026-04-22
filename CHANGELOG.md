# Changelog

## [1.0.0] - 2026-04-22

### Breaking

- **Upgraded to `donglora-client` 1.0.0-alpha.1 (DongLoRa Protocol v2 wire protocol).**
  Wire-incompatible with 0.x firmware. Connect/config/TX/RX all go through
  the new async `Dongle` API.
- **Radio loop is now a tokio task**, not a dedicated `std::thread`.
  Everything is async end-to-end.
- **`[radio]` config schema changes:**
  - `tx_power` renamed to `tx_power_dbm` and is now a plain `i8` (no
    `"max"` keyword — v1.0 exposes a concrete dBm range via `GET_INFO`).
  - `cad` field removed — CAD is a per-TX flag in v1.0, not a config
    property. The `[tx]` section's `skip_cad` (default false) controls it.
  - Default `sync_word` changed `0x3444` → `0x1424` to match the
    `SX126x` register encoding of `MeshCore`'s private sync byte
    (`RADIOLIB_SX126X_SYNC_WORD_PRIVATE`).

### Added

- **TX retry with backoff (new `[tx]` config section).** Every TX now
  runs through `Dongle::tx_with_retry`, which retries `TX_DONE(CHANNEL_BUSY)`
  and `ERR(EBUSY)` per spec `PROTOCOL.md §6.10` / §C.5.5. Defaults: 3
  attempts, randomized 20–100 ms backoff, doubling up to 500 ms cap,
  5 s per-attempt timeout. Configurable via `[tx]` keys `max_attempts`,
  `backoff_min_ms`, `backoff_max_ms`, `backoff_cap_ms`,
  `backoff_multiplier`, `per_attempt_timeout_secs`.
- **TUI packet log shows retry state.** In-flight retries render with a
  `↻` marker in yellow and an `N/M` attempt counter. Terminal TX failures
  render in red with `✗`. Post-retry success shows the final attempt count
  (`OK 2/3`).
- **Stats: `tx_retries` and `tx_failures` counters.** Lifetime totals,
  surfaced in the `StatsSnapshot`.
- **Measured airtime** from `TX_DONE` is now reported instead of a
  theoretical estimate, feeding the rate limiter with the real on-air
  duration.

### Removed

- The explicit 2-second liveness ping loop in the radio thread. The
  `Dongle` now runs a 500 ms keepalive task internally, matching spec
  §3.4's 1 s inactivity window with 2× margin.

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
