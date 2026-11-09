//! Platform dispatch: the list of known backends. The session worker
//! (jb-core `session`) polls them by the USB device that appears on
//! the bus, connects the first one and runs the scenario through the
//! `jb_core::backend::Backend` contract implemented by the platform
//! crates (jb-rockchip, jb-amlogic). A future platform = a new crate
//! implementing the contract + a line in `backends()`.

use jb_core::backend::Backend;

/// Every known platform in poll order. A future platform is added
/// here: recognized by its own USB device (`present`), the scenario
/// runs through the same contract.
pub(crate) fn backends() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(jb_rockchip::platform::Rockchip::new()),
        Box::new(jb_amlogic::platform::Amlogic::new()),
    ]
}
