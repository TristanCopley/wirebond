//! wirebond-core's integration tests, in one binary: each file in tests/ is a module of it, so the
//! driver they share (common) is compiled once and one executable is linked.

mod common;

mod alloc;
mod determinism;
mod harness;
mod pair;
mod vectors;
