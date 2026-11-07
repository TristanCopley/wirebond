//! wirebond 2.0: bond the networks cabled to a device into one path to an anchor of your own
//! (SPEC.md). The daemon (`wirebond run`), the CLI, the local API, the platform code and the
//! updater, in one binary.

// 2.0 skeleton (notes/2.0-build.md): until wave 2 wires the daemon's pumps and bond logic to the
// platform API, the contracts and the updater, much of these modules is unused.
// The node daemon's owner removes these allows when wave 2 joins.
#[allow(dead_code)]
mod api;
mod cli;
#[allow(dead_code)]
mod host;
mod node;
#[allow(dead_code)]
mod update;

/// The version and the commit are set by the release build: WIREBOND_VERSION=<tag>
/// WIREBOND_COMMIT=<sha> cargo build --release. A build without them is "dev".
fn main() {
    let version = option_env!("WIREBOND_VERSION").unwrap_or("dev");
    std::process::exit(cli::main(version, option_env!("WIREBOND_COMMIT").unwrap_or("")));
}
