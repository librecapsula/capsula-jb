//! Device bring-up and pipeline primitives shared by the operations.
//!
//! The production connect (`platform::connect_once`) runs bring-up with
//! [`Goal::UsbPlug`]: the loader persona (330d) identifies the column,
//! then it is reset into MaskROM and OUR usbplug is loaded into RAM
//! from the OTA loader (the RAM-patch address is only known for this
//! build, so the DRAM signatures are always verified). A column that
//! cannot run it — the first generation by its serial, or a batch with
//! an incompatible BootROM (the maskrom→loader hand-off fails on some
//! 0100 units) — comes back as [`BringUpError::FirstGeneration`]: the
//! caller asks the user and re-connects with [`Goal::StockLoader`]
//! over the stock u-boot rockusb loader (330d) alone.
//!
//! The stock loader is enough whenever nothing above the 32 MiB read
//! mask is touched (GPT lives at LBA 1, userdata goes through the
//! stock WRITE_LBA): the degraded flow and `--no-backup` runs keep it
//! and never download anything into RAM.

use crate::firstgen::is_first_generation;
use jb_core::config::Config;
use jb_core::model::Model;
use jb_core::progress::{Progress, ProgressFn};
use rockusb::loader::{LoaderSession, RESET_SUBCODE_MASKROM};
use rockusb::monitor::wait_download_mode;
use rockusb::{Error as UsbError, LoaderContainer, MaskRomSession, NusbTransport, Transport};
use std::time::Duration;

/// Bring-up error. `LoaderRejected`: the usbplug did not start — BootROM
/// silently rejected the loader signature (an unsupported board).
/// `FirstGeneration`: under [`Goal::UsbPlug`] the column cannot run our
/// loader — either the first generation (the serial says so up front) or
/// a batch with an incompatible BootROM (the maskrom→loader hand-off
/// fails on some 0100 units); the only way on is the stock loader, so
/// the caller must ask the user and wait for a 330d re-entry.
/// `UnknownIdentity`: the persona on the bus cannot identify the column
/// (no loader serial) — a fatal connect error: without the serial
/// neither the backup cache nor the first-generation path can be keyed;
/// the only way forward is a re-entry through the loader persona (330d),
/// the only one that reports the identity.
#[derive(Debug, thiserror::Error)]
pub enum BringUpError {
    #[error(transparent)]
    Usb(#[from] rockusb::Error),
    #[error("usbplug не запустился: BootROM отклонил лоадер")]
    LoaderRejected,
    #[error("первое поколение/несовместимый BootROM ({serial}): полный бэкап невозможен")]
    FirstGeneration { serial: String },
    #[error(
        "серийник колонки неизвестен: перезапустите её в режим лоадера \
         (выключить питание, зажать Recovery, включить) — без серийника \
         инструмент не может выбрать путь"
    )]
    UnknownIdentity,
}

/// The live device session after bring-up. `firstgen` can only be
/// true under [`Goal::StockLoader`]: the session is the stock u-boot
/// rockusb loader (330d) — either a genuine first-generation column
/// (gift serial) or a degraded re-connect the user consented to after
/// our loader was refused. The full backup is impossible on that path
/// (the 32 MiB read mask); userdata is written through the stock
/// WRITE_LBA.
pub struct BroughtUp<T: Transport> {
    pub session: LoaderSession<T>,
    /// The column identity serial for the backup key. Read exactly once
    /// (at the loader persona recognition); always identifying —
    /// bring_up refuses personas that cannot identify the column.
    pub serial: String,
    pub firstgen: bool,
}

/// The bus operations bring_up is built on — the seam that keeps the
/// state machine testable without hardware (production: [`UsbBus`]
/// over the rockusb bus helpers).
pub trait Bus {
    type T: Transport;
    /// Best persona on the bus right now (None = no download device).
    fn poll(&mut self) -> rockusb::Result<Option<rockusb::DeviceMode>>;
    /// iSerialNumber of the loader persona (330d), if it is on the bus.
    fn loader_serial(&mut self) -> Option<String>;
    /// Open a rockusb session: the loader persona, or a leftover usbplug
    /// (both speak the same protocol).
    fn open_loader(&mut self) -> rockusb::Result<LoaderSession<Self::T>>;
    /// Open the raw MaskROM transport (330e without USB-MSC).
    fn open_maskrom(&mut self) -> rockusb::Result<Self::T>;
    /// Wait for any download persona with debounce; prints the hint.
    fn wait_download_mode(
        &mut self,
        timeout: Option<Duration>,
        hint: &str,
    ) -> rockusb::Result<rockusb::DeviceMode>;
    /// Wait for a specific persona to appear on the bus.
    fn wait_mode(&mut self, mode: rockusb::DeviceMode, timeout: Duration) -> rockusb::Result<()>;
}

/// The loader-persona serial, read and normalized exactly once. USB
/// string descriptors can carry surrounding whitespace; both the
/// firstgen classification and the backup directory key must see the
/// same trimmed value.
fn normalize_serial(raw: Option<String>) -> String {
    raw.unwrap_or_default().trim().to_string()
}

/// The real bus: thin wrappers over the rockusb global helpers.
pub struct UsbBus;

impl Bus for UsbBus {
    type T = NusbTransport;

    fn poll(&mut self) -> rockusb::Result<Option<rockusb::DeviceMode>> {
        rockusb::current_mode()
    }

    fn loader_serial(&mut self) -> Option<String> {
        rockusb::device_serial(&rockusb::DeviceMode::Loader)
    }

    fn open_loader(&mut self) -> rockusb::Result<LoaderSession<NusbTransport>> {
        let t = NusbTransport::open(&rockusb::DeviceMode::Loader)
            .or_else(|_| NusbTransport::open(&rockusb::DeviceMode::UsbPlug))?;
        let (ep_in, ep_out) = (t.ep_in, t.ep_out);
        Ok(LoaderSession::new(t, ep_in, ep_out))
    }

    fn open_maskrom(&mut self) -> rockusb::Result<NusbTransport> {
        NusbTransport::open(&rockusb::DeviceMode::MaskRom)
    }

    fn wait_download_mode(
        &mut self,
        timeout: Option<Duration>,
        hint: &str,
    ) -> rockusb::Result<rockusb::DeviceMode> {
        wait_download_mode(timeout, hint)
    }

    fn wait_mode(&mut self, mode: rockusb::DeviceMode, timeout: Duration) -> rockusb::Result<()> {
        rockusb::monitor::wait_mode(mode, timeout)
    }
}

/// Does this USB serial identify the column? Gadget fallbacks report
/// empty or all-zero strings (the BootROM gadget: "000000000") — those
/// must never become a backup cache key.
fn is_identifying(serial: &str) -> bool {
    !serial.is_empty() && serial.chars().any(|c| c != '0')
}

/// What the caller needs from bring-up. The session persona follows
/// the goal. The first generation never runs our usbplug (its locked
/// BootROM rejects it) — with [`Goal::UsbPlug`] that is an error for
/// the caller to settle with the user, with [`Goal::StockLoader`] the
/// stock loader is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    /// Identity + the below-mask range (GPT at LBA 1, stock WRITE_LBA):
    /// the stock loader (330d) is enough — the degraded (first
    /// generation / incompatible-batch) re-connect and --no-backup
    /// runs. No MaskROM round-trip, no RAM download; a first-generation
    /// serial returns `firstgen = true` on the stock-loader session.
    StockLoader,
    /// A session that can read above the stock 32 MiB read mask (the
    /// full dump): our usbplug loaded into RAM. Never degrades on its
    /// own: a first-generation serial or a loader the BootROM refuses
    /// is [`BringUpError::FirstGeneration`] — the caller asks the user
    /// and re-connects with [`Goal::StockLoader`].
    UsbPlug,
}

/// Bring the board into a working session. Returns the session with the
/// column identity serial (the backup key) and the `firstgen` flag that
/// selects the further path. The serial is read exactly ONCE — when
/// the loader persona (330d) is recognized — and is kept for the whole
/// session; other personas never contribute (the BootROM gadget reports
/// zeros). Every session MUST have an identifying serial: a persona
/// that cannot identify the column (a leftover usbplug, a direct
/// MaskROM entry, a broken serial read) fails with
/// [`BringUpError::UnknownIdentity`] — a fatal connect error, the
/// caller's reconnect round-trip asks the user for a loader re-entry.
///
/// - Loader (330d) with a first-generation serial: [`Goal::StockLoader`]
///   keeps the stock loader — `firstgen = true`; [`Goal::UsbPlug`]
///   returns [`BringUpError::FirstGeneration`] without touching the
///   board (its locked BootROM rejects our loader anyway);
/// - Loader (330d) with a regular serial: [`Goal::StockLoader`] keeps
///   the stock loader; [`Goal::UsbPlug`] resets into MaskROM and loads
///   our loader — if it does not start (an incompatible-batch BootROM),
///   [`BringUpError::FirstGeneration`] names the column for the caller
///   to settle with the user;
/// - raw MaskROM (330e) with the serial already captured: download the
///   loader into RAM (flash untouched);
/// - empty bus / Linux gadget: print the hint and wait for a download mode.
pub fn bring_up<B: Bus>(
    bus: &mut B,
    config: &Config,
    loader: &LoaderContainer,
    wait: Option<Duration>,
    goal: Goal,
    progress: ProgressFn,
) -> Result<BroughtUp<B::T>, BringUpError> {
    let hint = config.connect_text(Model::Capsula);
    let mut serial: Option<String> = None;
    loop {
        let mode = bus.poll()?;
        let Some(mode) = mode else {
            bus.wait_download_mode(wait, hint)?;
            continue;
        };
        match mode {
            rockusb::DeviceMode::Loader => {
                // Read the identity exactly once, BEFORE the MaskROM
                // reset consumes the persona. A first-generation serial
                // or a stock-loader goal also keeps the whole scenario
                // on the stock loader: the reset itself would kick the
                // running stock loader — keep it instead.
                if serial.is_none() {
                    serial = Some(normalize_serial(bus.loader_serial()));
                }
                // A broken read or a gadget fallback (all zeros) keys to
                // no column: a fatal error — the caller's reconnect
                // round-trip asks for a loader re-entry (the next
                // attempt reads the serial again).
                let serial = match serial.as_deref() {
                    Some(s) if is_identifying(s) => s.to_string(),
                    _ => return Err(BringUpError::UnknownIdentity),
                };
                let firstgen = is_first_generation(&serial);
                // Under Goal::UsbPlug the first generation is the
                // caller's decision, not ours: its locked BootROM
                // rejects our loader, so ask (the caller shows the
                // notice) instead of silently degrading. The board is
                // NOT reset — it stays on the loader persona.
                if firstgen && goal == Goal::UsbPlug {
                    return Err(BringUpError::FirstGeneration { serial });
                }
                // Both stay on the stock loader: first generation
                // cannot run our usbplug at all, and a stock-loader
                // goal never reads above the stock read mask.
                if firstgen || goal == Goal::StockLoader {
                    let why = if firstgen {
                        "первое поколение"
                    } else {
                        "без выгрузки usbplug"
                    };
                    let session = attach_stock_loader(bus, progress, why)?;
                    return Ok(BroughtUp {
                        session,
                        serial,
                        firstgen,
                    });
                }
                // The board reached MaskROM, so the pipe is fine: a
                // loader that does not start there is an incompatible
                // BootROM (some 0100 batches) — the same first-
                // generation settlement, the column named for the
                // caller.
                let session = load_usbplug(bus, config, loader, progress)
                    .map_err(|e| first_generation_or(e, serial.clone()))?;
                return Ok(BroughtUp {
                    session,
                    serial,
                    firstgen: false,
                });
            }
            rockusb::DeviceMode::MaskRom => {
                // no serial read here: the identity is the loader serial
                // captured above; a direct MaskROM entry has none — the
                // persona cannot be keyed to a column, a fatal error
                let Some(ident) = serial.clone() else {
                    return Err(BringUpError::UnknownIdentity);
                };
                let session = download_usbplug(bus, config, loader, progress)
                    .map_err(|e| first_generation_or(e, ident.clone()))?;
                return Ok(BroughtUp {
                    session,
                    serial: ident,
                    firstgen: false,
                });
            }
            rockusb::DeviceMode::UsbPlug => {
                // A usbplug sighted here is a leftover from a previous
                // run: the flow that downloaded it returned directly,
                // so the serial is never known in this branch. Reusing
                // it would mean an unidentified column — a fatal error
                // (the board has to be power-cycled into 330d anyway).
                return Err(BringUpError::UnknownIdentity);
            }
            other => {
                log::info!("режим {}, жду download-режим", other.label());
                bus.wait_download_mode(wait, hint)?;
            }
        }
    }
}

/// Re-attach a KNOWN column after a mid-operation failure (a Windows
/// driver rebind, a wedged endpoint). Unlike [`bring_up`], every
/// persona is acceptable — the identity was read once already: the
/// loader persona re-reads the serial and refuses a different column,
/// a sighted usbplug is opened directly (its persona carries no
/// serial). First generation re-attaches the stock loader; a regular
/// column ends on a verified usbplug session ([`Goal::UsbPlug`]).
pub fn reattach<B: Bus>(
    bus: &mut B,
    config: &Config,
    loader: &LoaderContainer,
    serial: &str,
    firstgen: bool,
    progress: ProgressFn,
) -> Result<LoaderSession<B::T>, BringUpError> {
    let hint = config.connect_text(Model::Capsula);
    loop {
        let mode = bus.poll()?;
        let Some(mode) = mode else {
            bus.wait_download_mode(None, hint)?;
            continue;
        };
        match mode {
            rockusb::DeviceMode::Loader => {
                let seen = normalize_serial(bus.loader_serial());
                if seen != serial {
                    return Err(UsbError::WrongMode(format!(
                        "на шине другая колонка ({seen}), жду {serial}"
                    ))
                    .into());
                }
                if firstgen {
                    return attach_stock_loader(bus, progress, "первое поколение")
                        .map_err(BringUpError::Usb);
                }
                return load_usbplug(bus, config, loader, progress);
            }
            rockusb::DeviceMode::MaskRom => return download_usbplug(bus, config, loader, progress),
            // our own leftover usbplug from the interrupted attempt —
            // the identity is known, open it
            rockusb::DeviceMode::UsbPlug => {
                let mut s = bus.open_loader()?;
                verify_usbplug_signatures(&mut s, config, progress)?;
                return Ok(s);
            }
            other => {
                log::info!("режим {}, жду download-режим", other.label());
                bus.wait_download_mode(None, hint)?;
                continue;
            }
        }
    }
}

/// From a live loader persona (330d): sanity-check the pipe, reset into
/// MaskROM, download our loader and open the verified usbplug session.
/// A wedged loader endpoint fails every transfer and can only be fixed
/// by re-entering the loader mode.
fn load_usbplug<B: Bus>(
    bus: &mut B,
    config: &Config,
    loader: &LoaderContainer,
    progress: ProgressFn,
) -> Result<LoaderSession<B::T>, BringUpError> {
    progress(Progress::Step("лоадер (330d): сбрасываю в MaskROM".into()));
    let mut s = bus.open_loader()?;
    if let Err(e) = s.ready() {
        return Err(UsbError::WrongMode(format!(
            "лоадер (330d) не отвечает ({e}): передёрните \
             питание и войдите в режим лоадера заново"
        ))
        .into());
    }
    if let Err(e) = s.reset(RESET_SUBCODE_MASKROM) {
        return Err(UsbError::WrongMode(format!(
            "сброс в MaskROM отклонён ({e}): передёрните \
             питание и войдите в режим лоадера заново"
        ))
        .into());
    }
    bus.wait_mode(rockusb::DeviceMode::MaskRom, Duration::from_secs(15))?;
    download_usbplug(bus, config, loader, progress)
}

/// How long the downloaded loader has to come up as the usbplug
/// persona. On compatible hardware it enumerates within a couple of
/// seconds; the locked first-generation BootROM silently drops the
/// loader and the persona never appears — the maskrom entry itself
/// proves the pipe is fine, so this deadline separates "incompatible
/// BootROM" from a cable problem.
const USBPLUG_WAIT: Duration = Duration::from_secs(10);

/// A `bring_up` loader-start failure becomes [`BringUpError::
/// FirstGeneration`] naming the column; anything else (a dead 330d
/// session, a wrong mode) stays as-is for the caller to report.
fn first_generation_or(e: BringUpError, serial: String) -> BringUpError {
    match e {
        BringUpError::LoaderRejected => BringUpError::FirstGeneration { serial },
        other => other,
    }
}

/// The board is in MaskROM: download our loader into RAM (flash
/// untouched), wait for the usbplug persona and open a verified
/// session. A download rejected over the wire (a USB transfer reset —
/// the locked BootROM throws the loader out) is the same "did not
/// start" as the persona never appearing.
fn download_usbplug<B: Bus>(
    bus: &mut B,
    config: &Config,
    loader: &LoaderContainer,
    progress: ProgressFn,
) -> Result<LoaderSession<B::T>, BringUpError> {
    progress(Progress::Step(
        "MaskROM: загружаю лоадер в RAM (флеш не трогаю)".into(),
    ));
    {
        let mut m = MaskRomSession::new(bus.open_maskrom()?);
        m.download(loader).map_err(|_| {
            progress(Progress::Step("лоадер отвергнут на передаче".into()));
            BringUpError::LoaderRejected
        })?;
    }
    progress(Progress::Step("жду usbplug (до 10 с)".into()));
    if bus
        .wait_mode(rockusb::DeviceMode::UsbPlug, USBPLUG_WAIT)
        .is_err()
    {
        progress(Progress::Step("usbplug не поднялся".into()));
        return Err(BringUpError::LoaderRejected);
    }
    let mut s = bus.open_loader()?;
    verify_usbplug_signatures(&mut s, config, progress)?;
    Ok(s)
}

/// Open and sanity-check the stock rockusb loader (330d) and keep it as
/// the session. `why` names the flow in the progress line: a
/// first-generation column (its locked BootROM rejects our usbplug) or
/// a --no-backup run (nothing above the stock read mask is ever read,
/// so the MaskROM round-trip is pointless).
fn attach_stock_loader<B: Bus>(
    bus: &mut B,
    progress: ProgressFn,
    why: &str,
) -> rockusb::Result<LoaderSession<B::T>> {
    progress(Progress::Step(format!(
        "{why}: работаю через стоковый лоадер (330d)"
    )));
    let mut s = bus.open_loader()?;
    if let Err(e) = s.ready() {
        return Err(UsbError::WrongMode(format!(
            "стоковый лоадер (330d) не отвечает ({e}): передёрните питание \
             и войдите в режим лоадера заново"
        )));
    }
    Ok(s)
}

/// Verify the usbplug signatures in DRAM: proof that our loader build is
/// running, whose RAM-patch address is known.
fn verify_usbplug_signatures<T: Transport>(
    s: &mut LoaderSession<T>,
    config: &Config,
    progress: ProgressFn,
) -> rockusb::Result<()> {
    let sig1 = hex::decode(&config.usbplug.signature1_hex)
        .map_err(|_| UsbError::Loader("signature1_hex is not hex".into()))?;
    let sig2 = hex::decode(&config.usbplug.signature2_hex)
        .map_err(|_| UsbError::Loader("signature2_hex is not hex".into()))?;
    let mut buf = [0u8; 64];
    for (addr, want, which) in [
        (config.usbplug.signature1_addr, &sig1, "signature1"),
        (config.usbplug.signature2_addr, &sig2, "signature2"),
    ] {
        s.read_mem(addr, &mut buf[..want.len()])?;
        if &buf[..want.len()] != want.as_slice() {
            progress(Progress::Step(format!(
                "usbplug не совпал: {which} по адресу {addr:#x}"
            )));
            return Err(UsbError::WrongMode(
                "сигнатуры usbplug не совпали (чужой лоадер?)".into(),
            ));
        }
    }
    progress(Progress::Step("сигнатуры usbplug сверены".into()));
    Ok(())
}

/// Read and parse the on-device GPT (LBA1 + 33 sectors of entries).
/// Returns the parsed table together with the raw sectors (the backup
/// stores them as .gpt.bin).
pub fn read_gpt<T: Transport>(
    s: &mut LoaderSession<T>,
    progress: ProgressFn,
) -> rockusb::Result<(crate::gpt::Gpt, Vec<u8>)> {
    progress(Progress::Step("читаю GPT".into()));
    let mut buf = vec![0u8; 34 * 512];
    s.read_lba(1, &mut buf)?;
    let gpt = crate::gpt::Gpt::parse(&buf).map_err(|e| UsbError::Loader(format!("GPT: {e}")))?;
    Ok((gpt, buf))
}

#[cfg(test)]
mod tests {
    use super::is_identifying;

    #[test]
    fn identifying_serials() {
        assert!(is_identifying("01139EB3613ABD80")); // real chipid
        assert!(is_identifying("e5fe5a07c23ce39a")); // first generation
        assert!(is_identifying("123")); // short but real
    }

    #[test]
    fn dummy_serials_are_not_identifying() {
        // the BootROM gadget reports zeros; an unread serial is empty
        assert!(!is_identifying("000000000"));
        assert!(!is_identifying("0"));
        assert!(!is_identifying(""));
    }

    #[test]
    fn padded_serial_still_classifies_as_regular() {
        // a serial with surrounding whitespace must not flip the column
        // into the firstgen path: classification sees the trimmed value
        let raw = " 01139EB3613ABD80\r\n";
        let s = raw.trim();
        assert!(is_identifying(s));
        assert!(!crate::firstgen::is_first_generation(s));
        assert_eq!(
            jb_core::backup_store::BackupStore::dir_name(raw),
            "01139EB3613ABD80"
        );
    }
}
