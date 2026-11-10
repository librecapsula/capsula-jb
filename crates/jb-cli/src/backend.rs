//! Platform dispatch: the list of known backends handed to the session
//! worker (jb-core `session`). A future platform = a new crate
//! implementing the `jb_core::backend::Backend` contract + a line in
//! `backends()`.

use jb_core::backend::Backend;

/// Every known platform, in poll order.
pub(crate) fn backends() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(jb_rockchip::platform::Rockchip::new()),
        Box::new(jb_amlogic::platform::Amlogic::new()),
    ]
}
