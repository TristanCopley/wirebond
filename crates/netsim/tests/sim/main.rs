//! netsim's scenarios, one module per suite, in one test binary: each test binary links the engine
//! and the simulator again, so one binary links once, and its suites run side by side under one
//! WIREBOND_JOBS limit (`env::accept` and `env::par_runs` share it). A test is named by its
//! module: `cargo test -p wirebond-netsim --profile sim --test sim -- --exact accept::a1`.

mod accept;
mod frag;
mod fuzz;
mod jump;
mod repunch;
mod selftest;
mod sleep_ring;
mod start;
mod swarm;
