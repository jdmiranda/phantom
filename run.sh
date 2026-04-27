#!/bin/bash
# Launch Phantom — builds fresh, runs the supervisor.
set -e
cargo build --bin phantom --bin phantom-supervisor
exec cargo run --bin phantom-supervisor
