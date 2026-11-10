//! Device bring-up and pipeline primitives shared by the operations.
//!
//! Every regular-column operation that reads the flash above the stock
//! 32 MiB read mask (the full dump) runs over OUR usbplug loaded into
//! RAM from the OTA loader: the RAM-patch address is only known for
//! this build, so the DRAM signatures are always verified.
//!
//! When nothing above the mask is read, the stock u-boot rockusb loader
//! (330d) is enough (GPT lives at LBA 1, userdata goes through the
//! stock WRITE_LBA), so bring-up keeps it: first-generation columns
//! (their locked BootROM rejects our loader anyway) and `--no-backup`
//! runs. The caller then runs the no-backup userdata flow.

use crate::firstgen::is_first_generation;
use jb_core::config::Config;
use jb_core::progress::{Progress, ProgressFn};
use rockusb::loader::{LoaderSession, RESET_SUBCODE_MASKROM};
use rockusb::monitor::wait_download_mode;
use rockusb::{Error as UsbError, LoaderContainer, MaskRomSession, NusbTransport, Transport};
use std::time::{Duration, Instant};

/// Bring-up error. `LoaderRejected`: the usbplug did not start — BootROM
/// silently rejected the loader signature (an unsupported board).
#[derive(Debug, thiserror::Error)]
pub enum BringUpError {
    #[error(transparent)]
    Usb(#[from] rockusb::Error),
    #[error("usbplug не запустился: BootROM отклонил лоадер")]
    LoaderRejected,
}

/// The live device session after bring-up. `firstgen`: first
/// generation — the persona is the stock u-boot rockusb loader (330d
/// with a gift serial); a full backup is impossible on that hardware,
/// userdata is written through the stock WRITE_LBA. For `--no-backup`
/// runs the session is the stock loader too (regular columns included):
/// nothing above the stock read mask is ever read, so our usbplug is
/// not downloaded.
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

/// An unknown-identity persona: a leftover usbplug from a previous
/// run, a direct MaskROM entry, or a loader persona that did not yield
/// a real serial. Without the serial neither the backup cache nor the
/// first-generation path can be keyed, so no flow may proceed — the
/// only way forward is re-entry through the loader persona (330d), the
/// only one that reports the identity. Idempotent (prints once);
/// records when the wait started for the `wait` deadline.
fn refuse_unknown_identity(wait_loader: &mut Option<Instant>, progress: ProgressFn, hint: &str) {
    if wait_loader.is_some() {
        return;
    }
    *wait_loader = Some(Instant::now());
    let mut msg = "серийник колонки неизвестен: перезапустите её в режим лоадера \
                   (выключить питание, зажать Recovery, включить) — без серийника \
                   инструмент не может выбрать путь"
        .to_string();
    if !hint.is_empty() {
        msg.push('\n');
        msg.push_str(hint);
    }
    progress(Progress::Step(msg));
}

/// What the caller needs from bring-up. The session persona follows
/// the goal; the first generation always ends on the stock loader (its
/// locked BootROM rejects our usbplug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    /// Identity + the below-mask range (GPT at LBA 1, stock WRITE_LBA):
    /// the stock loader (330d) is enough — connect and --no-backup
    /// runs. No MaskROM round-trip, no RAM download.
    StockLoader,
    /// A session that can read above the stock 32 MiB read mask (the
    /// full dump): our usbplug loaded into RAM. First generation
    /// cannot and stays on the stock loader (`firstgen = true` tells
    /// the caller).
    UsbPlug,
}

/// Bring the board into a working session. Returns the session with the
/// column identity serial (the backup key) and the `firstgen` flag that
/// selects the further path. The serial is read exactly ONCE — when
/// the loader persona (330d) is recognized — and is kept for the whole
/// session; other personas never contribute (the BootROM gadget reports
/// zeros). Every session MUST have an identifying serial: a persona
/// that cannot identify the column (a leftover usbplug, a direct
/// MaskROM entry, a broken serial read) is refused with a re-entry
/// instruction, and only the loader persona is accepted afterwards.
///
/// - Loader (330d) with a first-generation serial: the stock loader
///   itself — `firstgen = true` (our loader would be rejected by its
///   locked BootROM);
/// - Loader (330d) with a regular serial: [`Goal::StockLoader`] keeps
///   the stock loader; [`Goal::UsbPlug`] resets into MaskROM and loads
///   our loader (the RAM-patch constants do not apply to the stock
///   u-boot usbplug);
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
    let hint = &config.hint.capsula_connect_text;
    let mut serial: Option<String> = None;
    // Set (with the refusal timestamp) once an unknown-identity persona
    // has been refused; afterwards only the loader persona (330d) may
    // proceed, whatever else appears on the bus.
    let mut wait_loader: Option<Instant> = None;
    loop {
        if let (Some(limit), Some(at)) = (wait, wait_loader)
            && at.elapsed() >= limit
        {
            return Err(UsbError::WrongMode(
                "не дождался повторного входа в режим лоадера (--wait)".into(),
            )
            .into());
        }
        let mode = bus.poll()?;
        let Some(mode) = mode else {
            if wait_loader.is_some() {
                std::thread::sleep(rockusb::monitor::POLL_INTERVAL);
            } else {
                bus.wait_download_mode(wait, hint)?;
            }
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
                if !is_identifying(serial.as_deref().unwrap_or("")) {
                    // The loader persona did not yield a real serial
                    // (broken read): drop it so the next loader
                    // recognition reads again, and ask for a re-entry.
                    serial = None;
                    refuse_unknown_identity(&mut wait_loader, progress, hint);
                    std::thread::sleep(rockusb::monitor::POLL_INTERVAL);
                    continue;
                }
                let serial = serial.clone().unwrap();
                let firstgen = is_first_generation(&serial);
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
                let session = load_usbplug(bus, config, loader, progress)?;
                return Ok(BroughtUp {
                    session,
                    serial,
                    firstgen: false,
                });
            }
            rockusb::DeviceMode::MaskRom => {
                // no serial read here: the identity is the loader serial
                // captured above; a direct MaskROM entry has none — the
                // persona cannot be keyed to a column, refuse it
                let Some(ident) = serial.clone() else {
                    refuse_unknown_identity(&mut wait_loader, progress, hint);
                    std::thread::sleep(rockusb::monitor::POLL_INTERVAL);
                    continue;
                };
                let session = download_usbplug(bus, config, loader, progress)?;
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
                // it would mean an unidentified column — refuse and
                // wait for the loader persona (the board has to be
                // power-cycled into 330d anyway).
                refuse_unknown_identity(&mut wait_loader, progress, hint);
                std::thread::sleep(rockusb::monitor::POLL_INTERVAL);
                continue;
            }
            other => {
                if wait_loader.is_some() {
                    // after a refusal only the loader persona counts;
                    // keep waiting for it quietly
                    std::thread::sleep(rockusb::monitor::POLL_INTERVAL);
                    continue;
                }
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
    let hint = &config.hint.capsula_connect_text;
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

/// The board is in MaskROM: download our loader into RAM (flash
/// untouched), wait for the usbplug persona and open a verified
/// session.
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
        m.download(loader)?;
    }
    progress(Progress::Step("жду usbplug (до 20 с)".into()));
    if bus
        .wait_mode(rockusb::DeviceMode::UsbPlug, Duration::from_secs(20))
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
