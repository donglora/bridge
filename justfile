# Default: run all checks
default: build

# Run all checks (fmt, clippy, test)
check: fmt-check clippy test audit

# Format code
fmt:
    cargo fmt

# Check formatting without changing files
fmt-check:
    cargo fmt --check

# Lint with clippy (pedantic + nursery)
clippy:
    cargo clippy --all-targets -- -D warnings

# Build release binary
build:
    cargo build --release

# Generate docs
doc:
    cargo doc --no-deps --open

# Run the bridge (TUI mode)
run *ARGS:
    cargo run --release -- {{ARGS}}

# Run in headless mode
run-headless *ARGS:
    cargo run --release -- --log-only {{ARGS}}

# Run the config wizard
config:
    cargo run --release -- config

# Security + license audit
audit:
    cargo deny check

# Run all tests
test: test-unit test-fuzz test-mutants

# Normal unit tests
test-unit:
    cargo nextest run

# Mutation testing
test-mutants *ARGS:
    cargo mutants {{ARGS}}

# Fuzz a target (requires nightly)
test-fuzz TARGET="fuzz_gossip_frame_decode" DURATION="60":
    cargo +nightly fuzz run {{TARGET}} -- -max_total_time={{DURATION}}

# Clean build artifacts
clean:
    cargo clean
