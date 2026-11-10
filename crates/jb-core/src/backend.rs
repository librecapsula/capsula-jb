//! The platform contract. The session worker (jb-core `session`, used
//! by both the CLI and the TUI) connects the backend of the model the
//! user chose up front (the CLI `--model` flag, the TUI menu) and runs
//! the scenario through the [`Platform`] hooks of the same object.
//! A new platform = a new `jb-<name>` crate implementing both traits
//! plus an arm in the binary's platform dispatch (`for_model`).

use crate::progress::ProgressFn;
use crate::session::Identity;
use anyhow::Result;
use std::time::Duration;

/// The typed failure of a [`Backend::connect`]. [`ConnectError::Attention`]
/// is not an error: the platform needs the user's explicit consent for a
/// degraded flow (rockchip: the full backup is impossible — the first
/// generation, or a batch whose BootROM refuses our loader) and a device
/// re-entry. The worker shows the notice, waits for the device to appear
/// again and retries connect; the backend itself narrows what it accepts
/// after the attention. A [`ConnectError::Fatal`] is reported to the user
/// with an offer to re-enter the device and retry; a refusal ends the
/// run (nothing has been touched — the connect is identity-only).
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Show the notice, ask the user to continue; on `Proceed` wait for
    /// the device again and call connect once more.
    #[error("connect: требуется подтверждение пользователя")]
    Attention { notice: Vec<String> },
    /// Report the error and offer a device re-entry; on `Proceed` wait
    /// for the device again and call connect once more, on `Abort` end
    /// the run.
    #[error(transparent)]
    Fatal(#[from] anyhow::Error),
}

/// One supported speaker platform. `Send` because the session worker
/// owns the backends on its own thread.
pub trait Backend: Send {
    /// Short platform name (for the dispatch log line).
    fn name(&self) -> &'static str;

    /// Is a device of this platform on the bus right now. The session
    /// worker uses this to pick the backend; the bus enumeration is
    /// the platform's business.
    fn present(&self) -> Result<bool>;

    /// The hint printed while the bus is empty (how to enter the
    /// download mode), if the platform has one.
    fn wait_hint(&self) -> Option<&str> {
        None
    }

    /// Bring the device into a working session and identify it: the
    /// column model, the backup key (serial / deviceid), the first-gen
    /// flag. Identity-only — no dump, no writes; the backup is settled
    /// later by [`crate::scenario::Platform::ensure_backup`]. May take
    /// long (driver installs, a loader download into RAM); the worker
    /// announces it with [`crate::session::Event::Connecting`]. A
    /// [`ConnectError::Attention`] return asks the user to accept a
    /// degraded flow and re-enter the device.
    fn connect(
        &mut self,
        wait: Option<Duration>,
        progress: ProgressFn,
    ) -> Result<Identity, ConnectError>;

    /// The connected device's [`crate::scenario::Platform`] hooks —
    /// the scenario skeleton runs over them.
    fn platform(&mut self) -> &mut dyn crate::scenario::Platform;
}
