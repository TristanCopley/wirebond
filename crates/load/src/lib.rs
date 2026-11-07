//! `load`: the load and echo tool of wirebond's system harness (SPEC §23.1 tier 3, §23.2), which
//! replaces 1.x's Go tool (tests/system/load). Standard library only. `tests/system/run.sh` drives
//! it in network namespaces; `load help` lists the commands.
//!
//! Every measurement prints one JSON line (for CI; `--out FILE` appends it to a results file) or,
//! with `--table`, a table for people; `load table FILE` prints a results file as tables. Each
//! `--check 'FIELD OP VALUE'` is a verdict on the line, and the exit status is 1 when one fails.
//! The round trip through an echo is what every delay measures, so sender and echo need no common
//! clock.

pub mod args;
pub mod echo;
pub mod json;
pub mod report;
pub mod stats;
pub mod tcp;
pub mod udp;
pub mod wait;

#[cfg(test)]
mod tests;
