//! Platform dispatch: the backend of the chosen model, handed to the
//! session worker (jb-core `session`). The model is picked up front —
//! the CLI `--model` flag, the TUI menu — and the worker polls only
//! that platform's bus presence. A future platform = a new crate
//! implementing the `jb_core::backend::Backend` contract + an arm in
//! [`for_model`].

use jb_core::backend::Backend;
use jb_core::session::Model;

/// The backend of the model.
pub(crate) fn for_model(model: Model) -> Box<dyn Backend> {
    match model {
        Model::Capsula => Box::new(jb_rockchip::platform::Rockchip::new()),
        Model::CapsulaMiniAmlogic => Box::new(jb_amlogic::platform::Amlogic::new()),
    }
}
