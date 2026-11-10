//! The rockchip [`jb_core::backend::Backend`] and
//! [`jb_core::scenario::Platform`] implementation. The scenario
//! skeleton (jb-core) drives the hooks; this module owns everything
//! device-specific: the Windows driver supervisor, the loader-persona
//! bring-up with the driver-rebind retries, the full dump, the ext4
//! image build and the WRITE_LBA write.
//!
//! On Windows the board hops between devnodes (330d loader, 330e
//! MaskROM/usbplug) and a fresh devnode may be grabbed by a vendor
//! Rockusb driver before our WinUSB package gets it; when that happens
//! the session rebinds the node and the bring-up — a re-entrant state
//! machine over the current bus state — is retried.

use anyhow::{Context, Result, anyhow, bail};
use jb_core::backend::{Backend, ConnectError};
use jb_core::backup_catalog::{self as catalog, BackupManifest};
use jb_core::backup_store::BackupStore;
use jb_core::config::Config;
use jb_core::format::humansize;
use jb_core::model::Model;
use jb_core::overlay::OverlayFile;
use jb_core::progress::ProgressFn;
use jb_core::scenario::{BackupState, Platform, Prepared, RestoreItem};
use jb_core::session::Identity;
use rockusb::NusbTransport;
use rockusb::loader::RESET_SUBCODE_NORMAL;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The gadget must stay on the bus this long to be considered alive:
/// the stock firmware blinks 0006 for under a second.
const GADGET_STABLE: Duration = Duration::from_secs(10);

/// The live device as the flow sees it (after connect). The `firstgen`
/// flag selects the degraded path inside the shared flow: the first
/// generation (gift serial) and columns whose BootROM refuses our
/// loader (some 0100 batches) — the session is the stock loader (330d),
/// the dump only covers the partitions fully below the 32 MiB read
/// mask and userdata is always built from scratch. `usbplug` tracks
/// which persona the session is: a regular column connects over our
/// usbplug (loaded at connect), the degraded path stays on the stock
/// loader.
struct Device {
    session: rockusb::LoaderSession<NusbTransport>,
    serial: String,
    /// The degraded path (stock loader, partial dump, userdata from
    /// scratch): the first generation OR an incompatible-batch column
    /// the user chose to continue with.
    firstgen: bool,
    /// The session is our usbplug in RAM (readable above the stock
    /// mask) — as opposed to the stock loader persona.
    usbplug: bool,
    /// A fresh backup was taken this run.
    dumped: bool,
}

/// How the next connect must run. `Fresh` is the default: the loader
/// persona is reset into MaskROM and our usbplug is loaded into RAM.
/// `StockOnly` is the accepted degraded flow (the user pressed
/// "continue"): ONLY the stock loader persona (330d) is accepted (see
/// [`Rockchip::present`]), nothing is ever downloaded into RAM again —
/// restarting the tool resets to `Fresh`.
#[derive(Default)]
enum Prep {
    #[default]
    Fresh,
    StockOnly {
        /// The column the user consented for; a different serial on
        /// re-entry restarts the fresh flow for the new column.
        serial: String,
    },
}

/// The rockchip platform. Holds the Windows driver session alive for
/// the whole scenario: the supervisor must rebind the board to WinUSB
/// during the userdata write as well.
pub struct Rockchip {
    config: Config,
    drivers: win_drv::Session,
    device: Option<Device>,
    gpt: Option<crate::gpt::Gpt>,
    wait: Option<Duration>,
    prep: Prep,
}

impl Rockchip {
    pub fn new() -> Self {
        Rockchip {
            config: Config::embedded(),
            drivers: win_drv::Session::new(),
            device: None,
            gpt: None,
            wait: None,
            prep: Prep::Fresh,
        }
    }
}

impl Default for Rockchip {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for Rockchip {
    fn name(&self) -> &'static str {
        "rockchip"
    }

    fn present(&self) -> Result<bool> {
        match rockusb::current_mode() {
            // In the accepted degraded flow only the stock loader
            // persona may proceed: after a refused loader the board
            // sits in MaskROM, and re-connecting to that would loop
            // the same failure forever.
            Ok(m) => Ok(match (&self.prep, m) {
                (Prep::StockOnly { .. }, mode) => mode == Some(rockusb::DeviceMode::Loader),
                (_, mode) => mode.is_some(),
            }),
            // a transient enumeration failure (a devnode mid-rebind on
            // Windows) must not abort the wait — report "not present"
            Err(e) => {
                log::warn!("перечисление USB не удалось ({e}) — попробую снова");
                Ok(false)
            }
        }
    }

    fn wait_hint(&self) -> Option<&str> {
        Some(self.config.connect_text(Model::Capsula))
    }

    fn connect(
        &mut self,
        wait: Option<Duration>,
        progress: ProgressFn,
    ) -> Result<Identity, ConnectError> {
        self.wait = wait;
        self.gpt = None;
        let dev = self.connect_device(wait, progress)?;
        let firstgen = crate::firstgen::is_first_generation(&dev.serial);
        if dev.firstgen {
            log::info!(
                "{} {} (стоковый лоадер): полный бэкап невозможен — \
                 маска 0xCC выше 32 МиБ; userdata будет собран с нуля",
                if firstgen {
                    "первое поколение"
                } else {
                    "несовместимый BootROM"
                },
                dev.serial
            );
        }
        let identity = Identity {
            model: Model::Capsula,
            key: dev.serial.clone(),
            // The true first-generation flag (the serial rule); an
            // incompatible-batch column is NOT the first generation —
            // its reason lives in the caveats.
            firstgen,
            caveats: if dev.firstgen {
                if firstgen {
                    vec![
                        "полный бэкап невозможен (BootROM залочен): дамп \
                         только разделов до boot_a, userdata с нуля"
                            .into(),
                    ]
                } else {
                    vec![
                        "полный бэкап невозможен: лоадер несовместим с этой \
                         колонкой — дамп только разделов до boot_a, userdata с нуля"
                            .into(),
                    ]
                }
            } else {
                vec![]
            },
        };
        self.device = Some(dev);
        Ok(identity)
    }

    fn platform(&mut self) -> &mut dyn Platform {
        self
    }
}

impl Platform for Rockchip {
    fn ensure_backup(&mut self, no_backup: bool, progress: ProgressFn) -> Result<Prepared> {
        let (serial, firstgen) = self
            .device
            .as_ref()
            .map(|d| (d.serial.clone(), d.firstgen))
            .ok_or_else(|| anyhow!("устройство не подключено"))?;
        let dir = backup_dir(&serial);
        if no_backup {
            log::info!(
                "--no-backup: бэкап не проверяю и не снимаю — userdata будет \
                 собран с нуля, текущее содержимое теряется"
            );
            return Ok(Prepared {
                base: None,
                dumped: false,
                dir: None,
                complete: !firstgen,
            });
        }
        // A verified backup of THIS column exists -> skip the dump and
        // go straight to the userdata write. The full checksum pass
        // happens right here, so a broken dump never serves as the
        // base of the only copy.
        if let Some(entry) = catalog::read(&dir) {
            match entry.verify() {
                Ok(()) => {
                    log::info!(
                        "бэкап уже есть в {} — дамп пропускаю (удалите каталог, \
                         чтобы снять заново)",
                        dir.display()
                    );
                    return Ok(Prepared {
                        base: if firstgen { None } else { image_base(&dir)? },
                        dumped: false,
                        dir: Some(dir),
                        complete: !firstgen,
                    });
                }
                Err(e) => {
                    log::warn!("бэкап не прошёл проверку ({e}) — снимаю заново");
                }
            }
        }
        let report = self.dump_with_retries(&dir, progress)?;
        self.seal_backup(&dir, &report)?;
        // The base of the new userdata image: the verified dump of this
        // column (regular path), or nothing when building from scratch.
        // Re-verifies the freshly sealed backup — the same gate as for
        // a reused capture.
        let base = if firstgen { None } else { image_base(&dir)? };
        Ok(Prepared {
            base,
            dumped: true,
            dir: Some(dir),
            complete: !firstgen,
        })
    }

    fn backup_state(&self) -> BackupState {
        let Some(dev) = &self.device else {
            return BackupState::None;
        };
        let dir = backup_dir(&dev.serial);
        // Manifest metadata only — the checksum pass runs in
        // ensure_backup before a capture becomes the base.
        match catalog::read(&dir) {
            Some(e) if e.manifest.status == catalog::Status::Complete => {
                BackupState::Verified { dir }
            }
            Some(e) => BackupState::Unusable {
                dir,
                why: if e.manifest.notes.is_empty() {
                    vec!["бэкап снят с оговорками".into()]
                } else {
                    e.manifest.notes.clone()
                },
            },
            None => BackupState::None,
        }
    }

    fn data_size(&mut self, progress: ProgressFn) -> Result<u64> {
        let dev = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        // The GPT gives the partition size and the flash address (the
        // dump file is exactly the GPT sector count, so the sizes
        // agree).
        let (gpt, _) = crate::session::read_gpt(&mut dev.session, progress)
            .context("не удалось прочитать GPT колонки")?;
        let part = gpt
            .find("userdata")
            .context("в GPT колонки нет раздела userdata")?;
        let size = part.sectors * 512;
        if dev.firstgen {
            log::info!("раздел userdata: {} (из GPT колонки)", humansize(size));
        }
        self.gpt = Some(gpt);
        Ok(size)
    }

    fn data_partition(&self) -> &'static str {
        "userdata"
    }

    fn describe_write(&self, fresh: bool, no_backup: bool) -> String {
        let firstgen = self.device.as_ref().map(|d| d.firstgen).unwrap_or(false);
        if firstgen {
            "оверлей adbd на чистый ext2 — полный бэкап невозможен \
             (первое поколение/несовместимый BootROM), userdata \
             собирается с нуля, содержимое теряется"
                .into()
        } else if no_backup {
            "оверлей adbd на чистый ext2 (--no-backup: бэкап не снимался, \
             текущее содержимое теряется)"
                .into()
        } else if fresh {
            "оверлей adbd на чистый ext2 (--format-userdata, содержимое теряется)".into()
        } else {
            "оверлей adbd поверх дампа, содержимое сохраняется".into()
        }
    }

    fn describe_restore(&self) -> String {
        "все целые разделы из верифицированного дампа + GPT и idblock — \
         оверлей джейлбрейка и всё, что записано после дампа, будет затёрто"
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
        crate::userdata::build_image(
            base.unwrap_or(Path::new("")),
            out,
            base.is_none(),
            size,
            files,
            progress,
        )
        .context("не удалось собрать образ userdata с оверлеем")
    }

    fn write_data_image(&mut self, image: &Path, progress: ProgressFn) -> Result<()> {
        let dev = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        let gpt = self
            .gpt
            .as_ref()
            .ok_or_else(|| anyhow!("GPT не прочитан"))?;
        crate::flash::write_partition_padded(&mut dev.session, gpt, "userdata", image, progress)
            .with_context(|| format!("колонка {}: запись userdata", dev.serial))
    }

    fn restore_set(&mut self) -> Result<(Vec<RestoreItem>, Vec<String>)> {
        let dev = self
            .device
            .as_ref()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        let dir = backup_dir(&dev.serial);
        let entry = catalog::read(&dir).ok_or_else(|| {
            anyhow!(
                "восстановление невозможно: нет верифицированного захвата {} \
                 для этой колонки; сначала снимите его — capsula-jb backup",
                dev.serial
            )
        })?;
        entry
            .verify()
            .context("бэкап не прошёл проверку SHA256 — снимите дамп заново")?;
        let mut items = Vec::new();
        let mut skipped = Vec::new();
        for img in &entry.manifest.images {
            if img.complete {
                items.push(RestoreItem {
                    partition: img.partition.clone(),
                    image: dir.join(&img.file),
                    size: img.size,
                });
            } else {
                skipped.push(img.partition.clone());
            }
        }
        Ok((items, skipped))
    }

    fn write_restore(&mut self, items: &[RestoreItem], progress: ProgressFn) -> Result<()> {
        // The LIVE table places the partitions (an image that no longer
        // fits — bad blocks grew the map — is an error, see the size
        // check in write_partition_padded); the GPT itself and the
        // idblock go LAST, restoring the table over itself is a no-op
        // when the maps are equal.
        if self.gpt.is_none() {
            let dev = self
                .device
                .as_mut()
                .ok_or_else(|| anyhow!("устройство не поднято"))?;
            let (gpt, _) = crate::session::read_gpt(&mut dev.session, progress)
                .context("не удалось прочитать GPT колонки")?;
            self.gpt = Some(gpt);
        }
        let serial = self
            .device
            .as_ref()
            .map(|d| d.serial.clone())
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        for item in items {
            let dev = self
                .device
                .as_mut()
                .ok_or_else(|| anyhow!("устройство не поднято"))?;
            let gpt = self.gpt.as_ref().expect("прочитан выше");
            crate::flash::write_partition_padded(
                &mut dev.session,
                gpt,
                &item.partition,
                &item.image,
                progress,
            )
            .with_context(|| format!("колонка {serial}: восстановление {}", item.partition))?;
        }
        // Everything that is not a partition: the GPT (LBA 1) and the
        // idblock (LBA 0x40), straight from the backup directory.
        let dir = backup_dir(&serial);
        let dev = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        for (file, lba, name) in [(".gpt.bin", 1u32, "gpt"), ("idblock.bin", 0x40, "idblock")] {
            let Ok(data) = std::fs::read(dir.join(file)) else {
                log::warn!("в бэкапе нет {file} — таблица/idblock не восстановлены");
                continue;
            };
            crate::flash::write_lba_image(&mut dev.session, lba, name, &data, progress)
                .with_context(|| format!("колонка {serial}: восстановление {name}"))?;
        }
        Ok(())
    }

    fn finish(&mut self, _progress: ProgressFn) -> Result<()> {
        let dev = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("устройство не поднято"))?;
        log::info!("перезагружаю колонку...");
        let _ = dev.session.reset(RESET_SUBCODE_NORMAL);
        wait_for_adb_gadget(self.wait)
    }

    fn report_backup(&self, prepared: &Prepared) {
        if !prepared.complete {
            log::info!(
                "частичный бэкап: наш лоадер на этой колонке не запускается — \
                 сняты только разделы до boot_a (от boot_a и выше \
                 стоковый лоадер читает 0xCC), userdata собирается с нуля"
            );
        }
        let serial = self
            .device
            .as_ref()
            .map(|d| d.serial.as_str())
            .unwrap_or("");
        let dir = prepared
            .dir
            .as_deref()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        if prepared.dumped {
            log::info!("готово: дампы {serial} — в {dir} (на колонку не писалось)");
        } else {
            log::info!(
                "бэкап {serial} уже есть в {dir} — делать нечего (удалите \
                 каталог, чтобы снять дамп заново)"
            );
        }
    }
}

/// Backup directory for a column: backup/capsule/<sanitized serial>
/// (per-model layout — identity keys of different platforms may
/// collide).
fn backup_dir(serial: &str) -> PathBuf {
    Path::new("backup")
        .join(Model::Capsula.name())
        .join(BackupStore::dir_name(serial))
}

/// The degraded-flow notice: the loader our usbplug needs does not run
/// on this column (the first generation, or an incompatible batch),
/// so the full backup is impossible and the user must consent. After
/// "continue" only the stock loader persona (330d) is accepted.
/// `by_serial`: the gift serial said it up front — the board was NOT
/// reset and already sits in the loader mode, so no re-entry needed;
/// otherwise the MaskROM reset left it stranded and the user must
/// power-cycle it back into the loader mode.
fn degraded_notice(by_serial: bool) -> Vec<String> {
    let mut notice = vec![
        if by_serial {
            "Кажется, у вас колонка первого поколения.".into()
        } else {
            "Кажется, у вас колонка первого поколения — или партии, на которой наш лоадер не запускается.".into()
        },
        "Полный бэкап на ней невозможен: выше 32 МиБ стоковый лоадер читает только мусор 0xCC.".into(),
        "Продолжить можно без полного бэкапа: снимутся только разделы до boot_a, userdata будет собран с нуля.".into(),
    ];
    notice.push(if by_serial {
        "Колонка уже в режиме лоадера — просто нажмите «Продолжить».".into()
    } else {
        "Выключите колонку, зажмите Recovery и включите — войдёт в режим лоадера, — затем нажмите «Продолжить».".into()
    });
    notice
}

impl Rockchip {
    /// Connect with the driver-rebind retries: the fresh flow loads our
    /// usbplug into RAM, the accepted degraded flow attaches the stock
    /// loader. An Attention (the degraded-flow consent) is NOT retried —
    /// it is for the user to settle, not a driver problem.
    fn connect_device(
        &mut self,
        wait: Option<Duration>,
        progress: ProgressFn,
    ) -> Result<Device, ConnectError> {
        if win_drv::any_rockchip_node_misbound() {
            log::info!("устройство Rockchip без WinUSB — ставлю драйвер");
            self.start_or_wait_drivers(Duration::from_secs(180));
        }

        const ATTEMPTS: u32 = 5;
        for attempt in 1..=ATTEMPTS {
            match self.connect_once(wait, progress) {
                Ok(s) => return Ok(s),
                attention @ Err(ConnectError::Attention { .. }) => return attention,
                Err(ConnectError::Fatal(e)) => {
                    let binding_error = win_drv::chain_is_binding_error(&e);
                    if binding_error && attempt < ATTEMPTS {
                        log::warn!(
                            "узел перехвачен чужим драйвером (попытка {attempt}/{ATTEMPTS}) — перевязываю"
                        );
                        self.start_or_wait_drivers(Duration::from_secs(120));
                        continue;
                    }
                    if binding_error {
                        // the driver supervisor could not win in ATTEMPTS tries
                        return Err(ConnectError::Fatal(e.context(format!(
                            "драйвер WinUSB не установился; лог: {}\n\
                             ручной путь: DRIVER-SETUP.txt (Zadig)",
                            self.drivers.log_path().display()
                        ))));
                    }
                    return Err(ConnectError::Fatal(e));
                }
            }
        }
        Err(ConnectError::Fatal(anyhow!(
            "перебор попыток перевязки драйвера"
        )))
    }

    /// One connect attempt (no retries). Fresh: bring the column up on
    /// our usbplug (MaskROM round-trip, RAM download); a column that
    /// cannot run it — the first generation by its serial, or a batch
    /// whose BootROM refuses the loader — becomes the degraded-flow
    /// Attention for the user. StockOnly (after "continue"): attach the
    /// stock loader, refusing a different column (that one gets the
    /// fresh flow for itself).
    fn connect_once(
        &mut self,
        wait: Option<Duration>,
        progress: ProgressFn,
    ) -> Result<Device, ConnectError> {
        let loader = crate::loader::embedded().context("не удалось разобрать вшитый лоадер")?;
        let mut bus = crate::session::UsbBus;
        loop {
            let degraded = matches!(self.prep, Prep::StockOnly { .. });
            let goal = if degraded {
                crate::session::Goal::StockLoader
            } else {
                crate::session::Goal::UsbPlug
            };
            let up =
                crate::session::bring_up(&mut bus, &self.config, &loader, wait, goal, progress)
                    .map_err(|e| match e {
                        crate::session::BringUpError::FirstGeneration { serial } => {
                            // The user decides; on "continue" ONLY this column's
                            // stock loader (330d re-entry) is accepted.
                            let by_serial = crate::firstgen::is_first_generation(&serial);
                            log::warn!(
                                "полный бэкап невозможен: колонка {serial} не запускает \
                         наш лоадер (первое поколение или несовместимая партия)"
                            );
                            self.prep = Prep::StockOnly { serial };
                            ConnectError::Attention {
                                notice: degraded_notice(by_serial),
                            }
                        }
                        crate::session::BringUpError::LoaderRejected => {
                            ConnectError::Fatal(anyhow!(
                                "usbplug не запустился: BootROM отклонил лоадер (неподдерживаемая \
                     плата — отпустите Recovery и войдите в режим лоадера заново)"
                            ))
                        }
                        e => ConnectError::Fatal(anyhow!(e)),
                    })?;
            if let Prep::StockOnly { serial } = &self.prep
                && &up.serial != serial
            {
                // a different column showed up in the loader persona:
                // the consent was for another one — run the fresh flow
                // for this column from scratch
                log::warn!(
                    "на шине другая колонка ({}, ждал {}): начинаю полный флоу заново",
                    up.serial,
                    serial
                );
                self.prep = Prep::Fresh;
                continue;
            }
            log::info!("серийник колонки: {}", up.serial);
            return Ok(Device {
                session: up.session,
                serial: up.serial,
                firstgen: degraded || up.firstgen,
                usbplug: !degraded,
                dumped: false,
            });
        }
    }

    /// Dump with the driver-rebind retries (the old dump_all skeleton:
    /// the switch to usbplug and the dump itself are retried when a
    /// Windows driver rebind kills the session). On a regular column
    /// the session runs our usbplug with the read-limit patch (fully
    /// readable dump); on first generation it stays on the stock
    /// loader (330d) and the dump is NOT intact: everything above the
    /// 32 MiB read mask reads back as 0xCC.
    fn dump_with_retries(
        &mut self,
        dir: &Path,
        progress: ProgressFn,
    ) -> Result<crate::backup::BackupReport> {
        const ATTEMPTS: u32 = 5;
        for attempt in 1..=ATTEMPTS {
            match self.dump_once(dir, progress) {
                Ok(report) => return Ok(report),
                Err(e) => {
                    let binding_error = win_drv::chain_is_binding_error(&e);
                    if binding_error && attempt < ATTEMPTS {
                        log::warn!(
                            "узел перехвачен чужим драйвером (попытка {attempt}/{ATTEMPTS}) — перевязываю"
                        );
                        self.start_or_wait_drivers(Duration::from_secs(120));
                        continue;
                    }
                    if binding_error {
                        // the driver supervisor could not win in ATTEMPTS tries
                        return Err(e.context(format!(
                            "драйвер WinUSB не установился; лог: {}\n\
                             ручной путь: DRIVER-SETUP.txt (Zadig)",
                            self.drivers.log_path().display()
                        )));
                    }
                    return Err(e);
                }
            }
        }
        bail!("перебор попыток перевязки драйвера")
    }

    /// One dump attempt (no retries): make sure the session persona can
    /// read the whole flash (usbplug on a regular column) and take the
    /// dump. Re-attaches from whatever persona the board was left in
    /// by a previous failed attempt.
    fn dump_once(
        &mut self,
        dir: &Path,
        progress: ProgressFn,
    ) -> Result<crate::backup::BackupReport> {
        let (serial, firstgen) = self
            .device
            .as_ref()
            .map(|d| (d.serial.clone(), d.firstgen))
            .ok_or_else(|| anyhow!("устройство не подключено"))?;
        if !firstgen {
            self.ensure_usbplug(progress)?;
        }
        let dev = self.device.as_mut().expect("проверено выше");
        let report = crate::backup::backup(&mut dev.session, &self.config, dir, progress, firstgen)
            .with_context(|| format!("колонка {serial}: дамп"))?;
        let _ = std::fs::write(dir.join("serial.txt"), &serial);
        self.device.as_mut().expect("проверено выше").dumped = true;
        Ok(report)
    }

    /// Seal a finished dump as the v2 backup: hash the small extras,
    /// publish `backup.json` atomically. The image checksums were
    /// streamed during the dump (see [`crate::backup::backup`]).
    fn seal_backup(
        &self,
        dir: &Path,
        report: &crate::backup::BackupReport,
    ) -> Result<catalog::BackupEntry> {
        let mut extras = Vec::new();
        for (file, role) in [
            (".gpt.bin", "gpt"),
            ("partition_table.tsv", "partition-table"),
            ("serial.txt", "serial"),
            ("idblock.bin", "idblock"),
        ] {
            let p = dir.join(file);
            if let Ok(meta) = std::fs::metadata(&p) {
                let sha256 = jb_core::backup_store::file_sha256(&p)?;
                extras.push(catalog::FileEntry {
                    file: file.to_string(),
                    sha256,
                    size: meta.len(),
                    role: role.to_string(),
                });
            }
        }
        let skipped = &report.skipped;
        let manifest = BackupManifest {
            format: catalog::FORMAT,
            model: Model::Capsula,
            key: dir
                .file_name()
                .map(|k| k.to_string_lossy().into_owned())
                .unwrap_or_default(),
            created_utc: jb_core::format::utc_now_iso(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            status: if skipped.is_empty() {
                catalog::Status::Complete
            } else {
                catalog::Status::Partial
            },
            notes: if skipped.is_empty() {
                vec![]
            } else {
                vec![format!(
                    "частичный дамп: наш лоадер не запускается на этой \
                     колонке (первое поколение или несовместимая партия) — \
                     сняты только разделы до boot_a (от boot_a и выше \
                     стоковый лоадер читает 0xCC), не снимались: {}",
                    skipped.join(", ")
                )]
            },
            images: report.images.clone(),
            extras,
        };
        catalog::publish(dir, manifest).context("публикация backup.json")
    }

    /// Switch the connected regular column from the stock loader to
    /// our usbplug session (the readable-dump persona). No-op when the
    /// session is usbplug already; otherwise re-attaches — from the
    /// loader persona, a leftover usbplug of ours or a raw MaskROM.
    fn ensure_usbplug(&mut self, progress: ProgressFn) -> Result<()> {
        let (serial, usbplug) = self
            .device
            .as_ref()
            .map(|d| (d.serial.clone(), d.usbplug))
            .ok_or_else(|| anyhow!("устройство не подключено"))?;
        if usbplug {
            return Ok(());
        }
        let loader = crate::loader::embedded().context("не удалось разобрать вшитый лоадер")?;
        let mut bus = crate::session::UsbBus;
        let s = crate::session::reattach(&mut bus, &self.config, &loader, &serial, false, progress)
            .map_err(|e| match e {
                crate::session::BringUpError::LoaderRejected => anyhow!(
                    "usbplug не запустился: BootROM отклонил лоадер (неподдерживаемая \
                 плата или первое поколение в MaskROM — отпустите Recovery и \
                 войдите в режим лоадера заново)"
                ),
                e => e.into(),
            })?;
        let dev = self.device.as_mut().expect("проверено выше");
        dev.session = s;
        dev.usbplug = true;
        Ok(())
    }

    /// Make sure the driver supervisor is running, then wait for the board
    /// to actually settle on WinUSB. The process is elevated from the very
    /// start (UAC manifest), so this is pure install time: certificates,
    /// the driver package import and the devnode restart take a few seconds
    /// on a first run.
    fn start_or_wait_drivers(&mut self, settle: Duration) {
        match self.drivers.ensure_supervision() {
            Ok(()) => {
                log::info!("ставлю драйвер WinUSB (до {} с)...", settle.as_secs());
                if !win_drv::wait_until_bound(settle) {
                    log::warn!(
                        "драйвер не установился за {} с (лог: {})",
                        settle.as_secs(),
                        self.drivers.log_path().display()
                    );
                }
            }
            Err(e) => {
                log::warn!(
                    "не удалось запустить драйверную сессию: {e}\n  \
                     ручной путь: DRIVER-SETUP.txt (Zadig)\n  лог: {}",
                    self.drivers.log_path().display()
                );
            }
        }
    }
}

/// The verified v2 backup of this column as the patch base, re-verified
/// before it becomes the base of the only copy.
fn image_base(dir: &Path) -> Result<Option<PathBuf>> {
    let entry = catalog::read(dir).ok_or_else(|| {
        anyhow!("нет валидного бэкапа этой колонки — запустите инструмент ещё раз")
    })?;
    entry
        .verify()
        .context("бэкап не прошёл проверку SHA256 — снимите дамп заново")?;
    Ok(Some(
        entry
            .image("userdata")
            .context("в бэкапе нет раздела userdata")?
            .0,
    ))
}

/// The shared tail of the flow: reboot the device and wait for the ADB
/// gadget to come up and stay.
fn wait_for_adb_gadget(wait: Option<Duration>) -> Result<()> {
    log::info!(
        "жду ADB-гаджет (2207:0006; должен продержаться {} с)...",
        GADGET_STABLE.as_secs()
    );
    rockusb::monitor::wait_gadget_stable(GADGET_STABLE, wait)
        .context("ADB-гаджет не поднялся (или мигнул и пропал) — проверьте загрузку колонки")?;
    log::info!("готово: ADB поднят — adb shell даёт root без авторизации");
    Ok(())
}
