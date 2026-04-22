# Research: Two-Process Supervisor Architecture

**Date**: 2026-04-21
**Status**: Implemented (phantom-supervisor crate)

---

## Decision

Two-process model inspired by Erlang/OTP. Lightweight supervisor process spawns, monitors, and restarts the main Phantom app via Unix domain socket with heartbeat protocol.

## Research Sources

- [Erlang/OTP Supervisor Behaviour](https://www.erlang.org/doc/system/sup_princ.html) — one_for_one, one_for_all, rest_for_one restart strategies
- [Who Supervises The Supervisors?](https://learnyousomeerlang.com/supervisors) — deep dive on OTP supervision trees
- [rust_supervisor crate](https://docs.rs/rust_supervisor) — Erlang-inspired process supervision in Rust
- [supertrees crate](https://docs.rs/supertrees) — process isolation via fork with restart policies
- [systemd Watchdog for Administrators](http://0pointer.de/blog/projects/watchdog.html) — heartbeat + kill + restart pattern
- [Dealing with process termination in Linux](https://iximiuz.com/en/posts/dealing-with-processes-termination-in-Linux/) — kill/wait patterns
- [Process spawning performance in Rust](https://kobzol.github.io/rust/2024/01/28/process-spawning-performance-in-rust.html) — spawn vs fork analysis

## Why Not Threads

A supervisor thread dies with the process. If the GPU hangs, the app OOMs, or a panic unwinds — the thread is gone. A separate process has its own address space, own memory, own fate.

See ARD-001 for the full decision record.
