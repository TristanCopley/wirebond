//! wirebond 2.0: bond the networks cabled to a device into one path to an anchor of your own
//! (SPEC.md). The daemon (`wirebond run`), the CLI, the local API, the platform code and the
//! updater, in one binary.

mod api;
mod cli;
mod host;
mod node;
mod update;

/// The version and the commit are set by the release build: WIREBOND_VERSION=<tag>
/// WIREBOND_COMMIT=<sha> cargo build --release. A build without them is "dev".
fn main() {
    let version = option_env!("WIREBOND_VERSION").unwrap_or("dev");
    std::process::exit(cli::main(version, option_env!("WIREBOND_COMMIT").unwrap_or("")));
}
