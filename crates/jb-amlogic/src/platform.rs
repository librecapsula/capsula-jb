//! The amlogic [`jb_core::backend::Backend`] and
//! [`jb_core::scenario::Platform`] implementation. The scenario
//! skeleton (jb-core) drives the hooks; this module owns everything
//! device-specific: the hot download-window catch, the checkpoint
//! capture across power cycles, the UBIFS image build and the chunked
//! write between windows.
//!
//! Platform notes measured on live hardware: the window is ~450 ms per
//! power-on (identify is accepted for ~450 ms), so the bus poll is at
//! the rockchip cadence; identify is sent immediately after open (it
//! also keeps the session alive); the first bulkcmd of a session is a
//! sacrificial `echo` (prime). The BootROM and the u-boot burning
//! gadget are indistinguishable on the bus (same VID:PID) — the stage
//! is told by identify, and only TPL (the u-boot gadget) carries the
//! storage commands. There is no signed WinUSB package for 1b8e:c003:
//! on Windows the board is moved to WinUSB once, manually (Zadig).

use crate::capture::{self, BackupReport, OpenedCapture, Pass, Status};
use crate::env;
use crate::write::{WriteState, write_data_pass};
use amlogic::{AmlogicSession, NusbTransport};
use anyhow::{Context, Result, anyhow, bail};
use jb_core::backend::{Backend, ConnectError};
use jb_core::config::Config;
use jb_core::format::humansize;
use jb_core::model::Model;
use jb_core::overlay::OverlayFile;
use jb_core::progress::ProgressFn;
use jb_core::scenario::{BackupState, Platform, Prepared, RestoreItem};
use jb_core::session::Identity;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The window-catch poll cadence: the u-boot window is ~450 ms, the bus
/// poll must confidently land inside (rockchip polls at 50 ms — the
/// same here).
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// The pause between open attempts in the hot loop (the flasher
/// reference spins at 10 ms).
const RETRY_PAUSE: Duration = Duration::from_millis(10);
/// How many consecutive failed open/identify attempts are tolerated
/// before a persistent problem (usually a driver on Windows) is
/// declared, instead of looping forever.
const MAX_OPEN_FAILURES: u32 = 10;
/// How long to wait for the column to leave the bus after a reset.
const REBOOT_WAIT: Duration = Duration::from_secs(30);
/// How many consecutive sessions without a byte of progress are
/// tolerated before a persistent problem (cable/driver/board) is
/// declared. A session with identify-keepalive moves tens of MiB
/// (measured 2026-10-31: 160 MiB), so zero progress is exactly a
/// cable/driver problem, not "too little time".
const MAX_STALLED_WINDOWS: u32 = 3;

/// Bytes of capture on disk (for the treading-water detection):
/// partition images (.img), reserved areas (.bin), checkpoints
/// (.partial) and the environment.
fn captured_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n.ends_with(".bin")
                        || n.ends_with(".img")
                        || n.ends_with(".partial")
                        || n == "env.txt"
                })
                .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                .sum()
        })
        .unwrap_or(0)
}

/// Read the u-boot environment over a fresh session (the identity
/// read). A session death at this step is a normal re-entry — this is
/// still only the first window — so up to three attempts with a
/// reconnect in between. Returns the (possibly re-entered) session and
/// the raw environment bytes.
fn capture_env(
    mut s: AmlogicSession<NusbTransport>,
    progress: &dyn Fn(jb_core::progress::Progress),
) -> Result<(AmlogicSession<NusbTransport>, Vec<u8>)> {
    let mut attempts = 0;
    loop {
        match env::capture_env_raw(&mut s) {
            Ok(raw) => return Ok((s, raw)),
            Err(e) => {
                attempts += 1;
                if attempts >= 3 {
                    return Err(e).context("съём окружения (identity)");
                }
                log::warn!("съём окружения не удался ({e:#}) — перезаход в окно");
                s = reconnect_window(s, progress)?;
            }
        }
    }
}

/// Re-enter the window: a warm cycle does not re-raise the gadget
/// (measured on a live board: `run try_auto_burn` + port reset produce
/// no re-enumeration) — ask for a power cycle and wait for the board on
/// the bus (flasher.sh `poll_for_device`).
fn reconnect_window(
    _s: AmlogicSession<NusbTransport>,
    progress: &dyn Fn(jb_core::progress::Progress),
) -> Result<AmlogicSession<NusbTransport>> {
    manual_reconnect(progress)
}

/// The manual re-entry: ask for a power cycle, wait for the board to
/// leave the bus (a dying gadget still hangs around and breaks the
/// bring-up open budget), then catch a fresh window.
fn manual_reconnect(
    progress: &dyn Fn(jb_core::progress::Progress),
) -> Result<AmlogicSession<NusbTransport>> {
    log::info!(
        "Выключите и снова включите питание колонки — USB-кабель не \
         трогайте; я поймаю её на шине."
    );
    let mut reminder = Instant::now();
    while amlogic::present().unwrap_or(false) {
        if reminder.elapsed() >= Duration::from_secs(30) {
            log::warn!("...плата всё ещё на шине: выдерните питание колонки и вставьте обратно");
            reminder = Instant::now();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    bring_up(None, progress)
}

/// The identity read from the u-boot environment keys the capture
/// directory; the environment is read from the device exactly once per
/// run (at connect, before the directory opens) and is recorded into
/// the capture, so a resume after a tool restart keeps the same
/// directory. A directory holding someone else's environment is
/// rejected (`capture::open_or_start`).
pub struct Amlogic {
    config: Config,
    session: Option<AmlogicSession<NusbTransport>>,
    /// The environment bytes read at connect — the identity source and
    /// the input of `capture::open_or_start`.
    env_raw: Option<Vec<u8>>,
    /// The capture directory and its status after ensure_backup (the
    /// data-size gate and the backup-only report read it).
    capture: Option<(PathBuf, Status)>,
    wait: Option<Duration>,
}

impl Amlogic {
    pub fn new() -> Self {
        Amlogic {
            config: Config::embedded(),
            session: None,
            env_raw: None,
            capture: None,
            wait: None,
        }
    }
}

impl Default for Amlogic {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for Amlogic {
    fn name(&self) -> &'static str {
        "amlogic"
    }

    fn present(&self) -> Result<bool> {
        match amlogic::present() {
            Ok(p) => Ok(p),
            // an instant enumeration failure (a node mid-rebind on
            // Windows) must not abort the wait
            Err(e) => {
                log::warn!("перечисление USB не удалось ({e}) — попробую снова");
                Ok(false)
            }
        }
    }

    fn wait_hint(&self) -> Option<&str> {
        Some(self.config.connect_text(Model::CapsulaMiniAmlogic))
    }

    fn connect(
        &mut self,
        wait: Option<Duration>,
        progress: ProgressFn,
    ) -> Result<Identity, ConnectError> {
        self.wait = wait;
        self.capture = None;
        self.env_raw = None;
        let s = bring_up(wait, progress)?;
        // identity: the environment before anything else; a session
        // death at this step is a normal re-entry, this is still only
        // the first window
        let (s, env_raw) = capture_env(s, progress)?;
        let deviceid = capture::device_id(&env_raw).context("съём окружения (identity)")?;
        log::info!("deviceid колонки: {deviceid}");
        self.session = Some(s);
        self.env_raw = Some(env_raw);
        Ok(Identity {
            model: Model::CapsulaMiniAmlogic,
            key: deviceid,
            firstgen: false,
            caveats: vec![],
        })
    }

    fn platform(&mut self) -> &mut dyn Platform {
        self
    }
}

impl Platform for Amlogic {
    fn ensure_backup(&mut self, no_backup: bool, progress: ProgressFn) -> Result<Prepared> {
        if no_backup {
            log::info!(
                "--no-backup: бэкап не снимаю — data будет собран с нуля, \
                 текущее содержимое теряется"
            );
            return Ok(Prepared {
                base: None,
                dumped: false,
                dir: None,
                complete: false,
            });
        }
        let env_raw = self
            .env_raw
            .clone()
            .ok_or_else(|| anyhow!("устройство не подключено"))?;
        let mut s = self
            .session
            .take()
            .ok_or_else(|| anyhow!("устройство не подключено"))?;

        let dir = match capture::open_or_start(&backup_root(), &env_raw)
            .context("каталог захвата Amlogic")?
        {
            OpenedCapture::Done(report) => {
                log::info!(
                    "бэкап уже есть в {} — дамп пропускаю (удалите каталог, \
                     чтобы снять заново)",
                    report.path.display()
                );
                let status = report.status;
                self.capture = Some((report.path.clone(), status));
                self.session = Some(s);
                return Ok(Prepared {
                    base: capture_base(&report),
                    dumped: false,
                    dir: Some(report.path),
                    complete: matches!(status, Status::Complete),
                });
            }
            OpenedCapture::Run(path) => path,
        };

        let mut last = captured_bytes(&dir);
        let mut stalls = 0u32;
        loop {
            match capture::capture_pass(&mut s, &dir, progress).context("проход захвата Amlogic")?
            {
                Pass::Done(report) => {
                    let status = report.status;
                    let path = report.path.clone();
                    let complete = matches!(status, Status::Complete);
                    self.capture = Some((path.clone(), status));
                    self.session = Some(s);
                    return Ok(Prepared {
                        base: capture_base(&report),
                        dumped: true,
                        dir: Some(path),
                        complete,
                    });
                }
                Pass::Interrupted(reason) => {
                    let now = captured_bytes(&dir);
                    stalls = if now > last { 0 } else { stalls + 1 };
                    last = last.max(now);
                    if stalls >= MAX_STALLED_WINDOWS {
                        self.session = Some(s);
                        bail!(
                            "окна идут, а захват стоит на месте ({now} байт): {reason}\n  \
                             проверьте кабель/плату; окно ловится, но данные не идут"
                        );
                    }
                    log::info!("{reason}");
                    s = reconnect_window(s, progress)
                        .context("перезаход в окно после исчерпания")?;
                }
            }
        }
    }

    fn backup_state(&self) -> BackupState {
        let Some(env_raw) = &self.env_raw else {
            return BackupState::None;
        };
        let Ok(deviceid) = capture::device_id(env_raw) else {
            return BackupState::None;
        };
        // The v2 manifest metadata first (no hashing here — the
        // checksum pass runs in ensure_backup); unfinished directories
        // are surfaced through the schema-2 checkpoint status.
        match jb_core::backup_catalog::open(
            Path::new("backup"),
            Model::CapsulaMiniAmlogic,
            &deviceid,
        ) {
            Some(e) if e.manifest.status == jb_core::backup_catalog::Status::Complete => {
                BackupState::Verified { dir: e.dir }
            }
            Some(e) => BackupState::Unusable {
                dir: e.dir,
                why: if e.manifest.notes.is_empty() {
                    vec!["захват снят с оговорками".into()]
                } else {
                    e.manifest.notes
                },
            },
            None => match capture::peek_status(&backup_root(), &deviceid) {
                Some((dir, Status::Running)) => BackupState::Unusable {
                    dir,
                    why: vec!["захват не завершён — продолжится при следующей операции".into()],
                },
                Some((dir, Status::Failed)) => BackupState::Unusable {
                    dir,
                    why: vec!["прошлый захват упал — удалите каталог".into()],
                },
                _ => BackupState::None,
            },
        }
    }

    fn data_size(&mut self, _progress: ProgressFn) -> Result<u64> {
        // The jailbreak-only base gate: a capture with bad blocks that
        // lost the data partition cannot become the base of a new
        // image. The backup-only flow tolerates it (report_backup says
        // so), so the gate lives here, not in prepare.
        if let Some((path, Status::Warnings)) = &self.capture
            && !path.join(capture::image_file("data")).is_file()
        {
            bail!(
                "раздел data прочитан не полностью (бед-блоки) — \
                     строить образ поверх неполного дампа нельзя. \
                     Повторите запуск (бед-блок может уйти с перечётом \
                     логической ёмкости) или используйте \
                     --format-userdata/--no-backup"
            );
        }
        let s = self
            .session
            .as_mut()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        Ok(s.partition_readable_size("data")
            .context("запрос логического размера data (store size)")? as u64)
    }

    fn data_partition(&self) -> &'static str {
        "data"
    }

    fn describe_write(&self, fresh: bool, no_backup: bool) -> String {
        if no_backup {
            "оверлей adbd на чистый ubifs (--no-backup: бэкап не снимался, \
             текущее содержимое теряется)"
                .into()
        } else if fresh {
            "оверлей adbd на чистый ubifs (--format-userdata, содержимое теряется)".into()
        } else {
            "оверлей adbd поверх захвата, содержимое сохраняется".into()
        }
    }

    fn describe_restore(&self) -> String {
        "все целые разделы из верифицированного захвата (AML-reserved и \
         окружение device-уникальны и не прошиваются) — оверлей \
         джейлбрейка и всё, что записано после захвата, будет затёрто"
            .into()
    }

    fn build_data_image(
        &self,
        base: Option<&Path>,
        out: &Path,
        size: u64,
        files: &[OverlayFile],
        progress: ProgressFn,
    ) -> Result<()> {
        crate::data_image::build_image(base, out, size, files, progress)
            .context("не удалось собрать образ раздела data с оверлеем")
    }

    fn write_data_image(&mut self, image: &Path, progress: ProgressFn) -> Result<()> {
        self.write_partition_windowed("data", image, progress)
    }

    fn restore_set(&mut self) -> Result<(Vec<RestoreItem>, Vec<String>)> {
        let env_raw = self
            .env_raw
            .clone()
            .ok_or_else(|| anyhow!("устройство не подключено"))?;
        let deviceid = capture::device_id(&env_raw)?;
        let entry = jb_core::backup_catalog::open(
            Path::new("backup"),
            Model::CapsulaMiniAmlogic,
            &deviceid,
        )
        .ok_or_else(|| {
            anyhow!(
                "восстановление невозможно: нет верифицированного захвата {} \
                     для этой колонки; сначала снимите его — capsula-jb backup",
                deviceid
            )
        })?;
        entry
            .verify()
            .context("захват не прошёл проверку SHA256 — снимите его заново")?;
        let mut items = Vec::new();
        let mut skipped = Vec::new();
        for img in &entry.manifest.images {
            // a shortened image (bad blocks) must not be flashed back:
            // writing half a partition corrupts it
            if img.complete {
                items.push(RestoreItem {
                    partition: img.partition.clone(),
                    image: entry.dir.join(&img.file),
                    size: img.size,
                });
            } else {
                skipped.push(img.partition.clone());
            }
        }
        Ok((items, skipped))
    }

    fn write_restore(&mut self, items: &[RestoreItem], progress: ProgressFn) -> Result<()> {
        // In the layout-map order (bootloader first, data last); the
        // device-unique AML-reserved areas and the environment are
        // NEVER written (see the module docs) — that is why they are
        // extras in the manifest, not images.
        for item in items {
            self.write_partition_windowed(&item.partition, &item.image, progress)?;
        }
        Ok(())
    }

    fn finish(&mut self, _progress: ProgressFn) -> Result<()> {
        let mut s = self
            .session
            .take()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        // reboot (u-boot reset; the gadget dies mid-status — expected)
        // and wait for the board to leave the bus
        log::info!("перезагружаю колонку...");
        let _ = s.bulk_cmd("reset");
        let start = Instant::now();
        while amlogic::present().unwrap_or(false) {
            if start.elapsed() >= REBOOT_WAIT {
                log::warn!(
                    "колонка не ушла с шины за {} с — проверьте её состояние",
                    REBOOT_WAIT.as_secs()
                );
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        log::info!(
            "готово: data прошит оверлеем adbd. Дождитесь загрузки колонки и \
             проверьте `adb devices` (PID живого Linux-гаджета amlogic-мин \
             инструмент не знает — следим только за 1b8e:c003)."
        );
        Ok(())
    }

    fn report_backup(&self, prepared: &Prepared) {
        if !prepared.dumped {
            log::info!("nothing to do");
            return;
        }
        let Some((path, status)) = &self.capture else {
            return;
        };
        match status {
            Status::Complete => log::info!(
                "готово: полный захват {} (на колонку не писалось)",
                path.display()
            ),
            Status::Warnings => log::info!(
                "готово: захват {} — с укороченными разделами (бед-блоки; \
                 см. manifest.json и файлы .partial; на колонку не писалось)",
                path.display()
            ),
            other => log::warn!("неожиданный статус захвата: {other:?}"),
        }
    }
}

impl Amlogic {
    /// The chunked partition write between power-cycle windows (the
    /// jailbreak's data write and every partition of the full
    /// restore): erase the full PHYSICAL range, write the image in
    /// chunks with an identify-keepalive each, and on a window death
    /// reconnect and restart the partition from scratch (the erase
    /// always covers the whole range — see write.rs for the erase/write
    /// arithmetic). A session death at a pass boundary surfaces as the
    /// first failed keepalive/erase of the next pass — the same loop
    /// handles it.
    fn write_partition_windowed(
        &mut self,
        partition: &str,
        image: &Path,
        progress: ProgressFn,
    ) -> Result<()> {
        let mut s = self
            .session
            .take()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        let (_, off, physical) = amlogic::partition(partition)
            .ok_or_else(|| anyhow!("в карте разделов нет {partition}"))?;
        let image_len = std::fs::metadata(image)?.len() as usize;
        // the deterministic checks of write_data_pass, hoisted: a
        // retry loop must not spin on them
        anyhow::ensure!(
            image_len > 0 && image_len.is_multiple_of(amlogic::nand::PAGE_SIZE as usize),
            "{partition}: размер образа некратен странице"
        );
        anyhow::ensure!(
            physical as usize >= image_len,
            "{partition}: образ больше физического раздела"
        );
        let mut file = std::fs::File::open(image)?;
        log::info!(
            "запись {partition}: {} ({})",
            humansize(image_len as u64),
            image.display()
        );
        progress(jb_core::progress::Progress::TransferStart {
            name: partition.into(),
            bytes: image_len as u64,
        });
        let mut st = WriteState::fresh();
        let mut last = 0usize;
        let mut stalls = 0u32;
        loop {
            let done = write_data_pass(
                &mut s,
                &mut file,
                off,
                physical,
                image_len,
                &mut st,
                &|done| {
                    progress(jb_core::progress::Progress::TransferProgress {
                        name: partition.into(),
                        done: done as u64,
                        total: image_len as u64,
                    });
                },
            )
            .with_context(|| format!("проход записи {partition}"))?;
            if done {
                break;
            }
            stalls = if st.written > last { 0 } else { stalls + 1 };
            last = last.max(st.written);
            if stalls >= MAX_STALLED_WINDOWS {
                self.session = Some(s);
                bail!(
                    "окна идут, а запись {partition} стоит на месте ({}/{}): \
                     проверьте кабель/плату",
                    humansize(st.written as u64),
                    humansize(image_len as u64)
                );
            }
            log::info!(
                "записано {} из {} ({partition}) — нужно свежее окно",
                humansize(st.written as u64),
                humansize(image_len as u64)
            );
            s = reconnect_window(s, progress)
                .context("перезаход в окно после исчерпания записи")?;
        }
        progress(jb_core::progress::Progress::TransferDone {
            name: partition.into(),
        });
        self.session = Some(s);
        Ok(())
    }
}

/// The capture's data partition as the patch base: published as the
/// numbered image (07_data.img) only when fully read. A Warnings
/// capture with a full data image still serves (short sections are
/// bad blocks elsewhere); a missing one yields None — the
/// jailbreak-only gate in `data_size` then rejects the run.
fn capture_base(report: &BackupReport) -> Option<PathBuf> {
    let data = report.path.join(capture::image_file("data"));
    match report.status {
        Status::Complete => Some(data),
        Status::Warnings if data.is_file() => Some(data),
        _ => None,
    }
}

/// The amlogic backup root: backup/capsule-mini/ (per-model layout —
/// identity keys of different platforms may collide).
fn backup_root() -> PathBuf {
    Path::new("backup").join(Model::CapsulaMiniAmlogic.name())
}

/// Raise a session: catch the 1b8e:c003 window (open the device
/// directly on every pass — one bus enumeration per attempt, no
/// presence check), identify immediately, prime (a sacrificial
/// `echo`), require the TPL stage (burning commands exist only in the
/// u-boot gadget; the Mini's BootROM is password-locked and useless
/// for the scenario). "No device" = an empty bus; other errors
/// accumulate into a budget — a persistent failure (usually a driver)
/// surfaces instead of an endless loop.
pub fn bring_up(
    deadline: Option<Duration>,
    progress: &dyn Fn(jb_core::progress::Progress),
) -> Result<AmlogicSession<NusbTransport>> {
    let start = Instant::now();
    let mut open_failures = 0u32;
    let mut seen = false;
    let mut hint_shown = false;
    loop {
        match open_and_identify(progress) {
            Ok((session, soc)) => {
                require_tpl(&soc)?;
                return Ok(session);
            }
            Err(amlogic::Error::DeviceNotFound) => {
                if seen {
                    log::info!("usb: 1b8e:c003 пропал (+{}мс)", start.elapsed().as_millis());
                    seen = false;
                    if !hint_shown {
                        log::info!(
                            "окно download-режима закрылось — выключите и снова \
                             включите подключённую колонку, я поймаю следующее"
                        );
                        hint_shown = true;
                    }
                }
                std::thread::sleep(RETRY_PAUSE);
            }
            Err(e @ amlogic::Error::MultipleDevices) => {
                bail!("{e}: подключите ровно одну колонку");
            }
            Err(e) => {
                if !seen {
                    log::info!(
                        "usb: 1b8e:c003 появился (+{}мс)",
                        start.elapsed().as_millis()
                    );
                    seen = true;
                }
                open_failures += 1;
                if open_failures >= MAX_OPEN_FAILURES {
                    bail!(
                        "устройство Amlogic на шине есть, но не открывается: {e}.\n  \
                         на Windows переведите 1b8e:c003 на WinUSB вручную (Zadig, \
                         см. DRIVER-SETUP.txt); macOS/Linux — проверьте права \
                         (udev-правило из архива)"
                    );
                }
                log::warn!(
                    "open/identify не удался ({e}) (+{}мс); пробую ещё",
                    start.elapsed().as_millis()
                );
            }
        }
        if let Some(d) = deadline
            && start.elapsed() >= d
        {
            bail!("колонка Amlogic не появилась на шине (--wait)");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Open the device and identify it: identify before any pause (the
/// prime briefly retries an unavailable bulk channel, and the BootROM
/// has no burning channel at all). The first bulkcmd of a session may
/// lose its first bytes (flasher.sh prime_bulk) — the sacrificial
/// command wastes them; its failure = a dead pipe, continuing is
/// pointless (the session would die on the first status anyway).
pub fn open_and_identify(
    progress: &dyn Fn(jb_core::progress::Progress),
) -> amlogic::Result<(AmlogicSession<NusbTransport>, amlogic::SocId)> {
    let t = Instant::now();
    let mut s = AmlogicSession::open()?;
    let opened = t.elapsed();
    let soc = s.identify_and_prime()?;
    log::info!("amlogic: open +{opened:?}, identify +{:?}", t.elapsed());
    progress(jb_core::progress::Progress::Step(format!(
        "amlogic download device: {soc}"
    )));
    Ok((s, soc))
}

/// Burning commands (store/download/upload/env) exist only in the
/// u-boot gadget; the BootROM answers bulkcmds with silence.
pub fn require_tpl(soc: &amlogic::SocId) -> Result<()> {
    if soc.stage() != amlogic::Stage::Tpl {
        bail!(
            "устройство в стадии {} — storage-команды доступны только из \
             u-boot-гаджета; передёрните питание колонки, чтобы поймать её \
             download-окно",
            soc.stage().label()
        );
    }
    Ok(())
}
