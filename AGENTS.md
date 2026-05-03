# AGENTS.md

## Build

- `cargo build --release` — debug builds may not fit in flash/RAM.
- The default target (`thumbv8m.main-none-eabihf`) is preconfigured in `.cargo/config.toml`. No `--target` flag needed.
- **Do not pass `--features ram_rom`, `flash_rom`, or `psram_rom` to cargo.** These are auto-set by `build.rs` from the `ROM_LOCATION` env var in `.env` (via `cargo:rustc-cfg`). Passing them on the CLI may conflict.
- `cargo run` flashes the device via `picotool load -u -v -x -t elf` (configured as the runner).

## Configuration

- **`.env` is gitignored.** Copy `.env.example` to `.env` before building. Required vars: `ROM_LOCATION`, `DISPLAY_DRIVER`, `DISPLAY_WIDTH`, `DISPLAY_HEIGHT`.
- Pin assignments live in `pin_mapping.env`. To override, set the `CUSTOM_PIN_MAP` env var to a path. Both files are read by `build.rs` and injected as `PIN_*` env vars into the crate at compile time.
- `DISPLAY_DRIVER` in `.env` selects the mipidsi model (e.g. `mipidsi::models::ILI9341Rgb565`). `build.rs` generates `use {driver} as DisplayDriver;` in `OUT_DIR`.

## Architecture

- `no_std` + `no_alloc` embedded binary for RP2350 (Pi Pico 2). Single crate, single entrypoint: `src/main.rs`.
- `src/gameboy/` — emulation layer (wraps `gb-core` with an iterator-based scanline handler, LRU-cached SD ROM manager, display/audio traits).
- `src/hardware/` — RP2350 peripherals: SPI+SD card, flash programming, PSRAM init, PIO-based I2S audio and SPI display interface, DMA streaming, screen scaling.
- `src/ui/` — ROM selection menu and loading screen rendered via `embedded-graphics`.
- Feature-gated ROM loading paths (via `ROM_LOCATION` env):
  - `RAM` — loads ROM from SD into an LRU bank cache (`const-lru`, cached, uses heap).
  - `FLASH` — writes ROM into on-chip flash via bootrom functions (persistent, up to ~3.5 MB).
  - `PSRAM` — detects and initializes external PSRAM (Pimoroni Pico Plus 2), loads ROM there.

## Dependencies

- Uses patched git forks:
  - `rp235x-hal` from `https://github.com/rp-rs/rp-hal.git` (rev pinned in `Cargo.toml`).
  - `gb-core` from `https://github.com/Altaflux/rust-gb.git` (rev pinned).
  - `dotenvy` from `https://github.com/allan2/dotenvy.git` (build-dep).
- Do not switch these to crates.io versions without verifying compatibility.

## Testing / Linting

- **There are no tests.** This is bare-metal embedded code; `cargo test` will not work. Do not attempt to add or run tests.
- No `rustfmt.toml` or `clippy.toml` exists. `cargo fmt` and `cargo clippy` use Rust defaults.

## Logging

- Uses `defmt` (not `log`). `DEFMT_LOG=info` is set in `.cargo/config.toml`. Change to `trace`/`debug` for more verbose output. The defmt serial transport sends logs over UART (pins from `PIN_UART_TX`/`PIN_UART_RX` in `pin_mapping.env`).

## Other conventions

- `overflow-checks = true` in release profile (unusual for embedded; intentional for correctness).
- `Cargo.lock` is gitignored (non-standard for a binary crate; do not commit it).
