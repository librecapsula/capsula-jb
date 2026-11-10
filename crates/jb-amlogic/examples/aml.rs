//! aml — pass-through diagnostics of the Capsula Mini burning session:
//! identify the board and talk to the stock u-boot directly. A debug
//! tool (was the `capsula-jb aml` subcommand), lives in examples —
//! not part of the user-facing flow.
//!
//!   cargo run -p jb-amlogic --example aml --release -- "printenv bootdelay"

use jb_amlogic::diag;
use jb_core::progress::Progress;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cmds = std::env::args().skip(1).collect::<Vec<_>>();
    let progress = |ev: Progress| {
        if let Progress::Step(msg) = ev {
            println!("{msg}");
        }
    };
    // no --wait: catching the download window is the point of the tool
    diag::passthrough(None, &cmds, &progress)
}
