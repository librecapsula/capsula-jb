//! Full GPT partition dump (the LibreRD method).
//! Bring-up (MaskROM -> loader into RAM -> signatures) is done by the
//! session BEFORE this is called.

use crate::session::read_gpt;
use jb_core::backup_catalog::ImageEntry;
use jb_core::config::Config;
use jb_core::progress::{Progress, ProgressFn};
use rockusb::loader::LoaderSession;
use rockusb::{Error as UsbError, Transport};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

/// Sectors per flash read (4 MiB).
pub const CHUNK_SECTORS: u32 = 8192;
/// Guard sector for the 0xCC check: first one beyond the stock read limit.
pub const GUARD_LBA: u32 = 0x10000;

#[derive(Debug)]
pub struct BackupReport {
    pub out_dir: std::path::PathBuf,
    pub partitions: usize,
    pub sectors_total: u64,
    /// The limit value before the RAM patch; None when running on the
    /// stock loader (the degraded flow) — there is no patch there.
    pub read_limit_old: Option<u32>,
    /// The dumped images with their streamed checksums — the caller
    /// seals them into the v2 manifest (`backup_catalog::publish`).
    pub images: Vec<ImageEntry>,
    /// Partitions NOT dumped: on the stock loader the capture cuts at
    /// boot_a, and everything reaching past the read mask is garbage —
    /// those partitions are skipped whole instead of dumping 0xCC
    /// waste (on the Capsule map that leaves uboot, trust, misc).
    pub skipped: Vec<String>,
}

/// Patch the usbplug read limit in RAM: without it,
/// everything above 0x10000 sectors reads back as 0xCC. Returns the
/// previous value.
pub fn patch_read_limit<T: Transport>(
    s: &mut LoaderSession<T>,
    config: &Config,
    progress: ProgressFn,
) -> rockusb::Result<u32> {
    let info = s.flash_detect()?;
    let total = info.sector_total;
    if total <= 34 {
        return Err(UsbError::WrongMode(format!(
            "usbplug отдал бредовый размер флеша: {total} секторов"
        )));
    }
    let addr = config.usbplug.read_limit_addr;
    let mut cur = [0u8; 4];
    s.read_mem(addr, &mut cur)?;
    let old = u32::from_le_bytes(cur);
    if old != total {
        progress(Progress::Step(format!(
            "предел чтения: {old} → {total} секторов (RAM-патч usbplug)"
        )));
        s.write_mem(addr, &total.to_le_bytes())?;
        let mut chk = [0u8; 4];
        s.read_mem(addr, &mut chk)?;
        if chk != total.to_le_bytes() {
            return Err(UsbError::WrongMode(
                "RAM-патч не применился (проверочное чтение не совпало)".into(),
            ));
        }
    } else {
        progress(Progress::Step(format!(
            "предел чтения уже расширен: {total} секторов"
        )));
    }
    Ok(old)
}

/// 0xCC guard: read one sector beyond the old limit; if the mask is
/// still active the backup would be garbage — refuse.
pub fn cc_guard<T: Transport>(
    s: &mut LoaderSession<T>,
    progress: ProgressFn,
) -> rockusb::Result<()> {
    let mut buf = vec![0u8; 512];
    s.read_lba(GUARD_LBA, &mut buf)?;
    if buf.iter().all(|&b| b == 0xcc) {
        return Err(UsbError::WrongMode(format!(
            "LBA {GUARD_LBA:#x} читается как 0xCC — RAM-патч не действует"
        )));
    }
    progress(Progress::Step("маска 0xCC снята".into()));
    Ok(())
}

/// Back up the partitions into `out_dir`. The caller has done
/// bring_up: signatures verified, and seals the v2 manifest from the
/// returned report (`backup_catalog::publish`) after writing the
/// directory extras (serial.txt).
///
/// On `stock_loader` (the degraded flow: first generation or an
/// incompatible-batch BootROM, 330d u-boot) neither the RAM patch nor
/// the guard is possible: everything above the stock read limit reads
/// back as 0xCC. The partial capture cuts at boot_a: everything from
/// boot_a up (and anything else reaching past the read mask, like the
/// straddling boot_b) is NOT dumped at all — the names go to `skipped`
/// and the run ends with a partial-backup warning. What remains is the
/// partitions before boot_a (on the Capsule map: uboot, trust, misc),
/// each intact and restorable.
pub fn backup<T: Transport>(
    s: &mut LoaderSession<T>,
    config: &Config,
    out_dir: &Path,
    progress: ProgressFn,
    stock_loader: bool,
) -> rockusb::Result<BackupReport> {
    let old_limit = if stock_loader {
        None
    } else {
        let old = patch_read_limit(s, config, progress)?;
        cc_guard(s, progress)?;
        Some(old)
    };

    let (gpt, gpt_raw) = read_gpt(s, progress)?;

    std::fs::create_dir_all(out_dir)?;
    std::fs::write(out_dir.join(".gpt.bin"), &gpt_raw)?;
    std::fs::write(out_dir.join("partition_table.tsv"), gpt.to_tsv())?;
    // The idblock — the BootROM's flash image table at LBA 0x40 —
    // lives OUTSIDE every GPT partition (the gap between the table
    // and the first partition), so the GPT-driven dump never sees it;
    // read it separately. 4 sectors, far below the stock read mask —
    // works on both the usbplug and the stock loader (first
    // generation). Documentation-grade: nothing flashes it back.
    progress(Progress::Step("idblock (LBA 0x40)".into()));
    let mut idblock = vec![0u8; 4 * 512];
    s.read_lba(0x40, &mut idblock)?;
    std::fs::write(out_dir.join("idblock.bin"), &idblock)?;
    // The v2 manifest must go before the first partition is written:
    // an interrupted re-dump into an old directory would otherwise
    // leave a stale backup.json next to fresh-but-partial files.
    let _ = std::fs::remove_file(out_dir.join(jb_core::backup_catalog::MANIFEST));
    let _ =
        std::fs::remove_file(out_dir.join(format!("{}.tmp", jb_core::backup_catalog::MANIFEST)));

    let info = s.flash_detect()?;
    progress(Progress::Step(format!(
        "разделов: {}, флеш: {} секторов",
        gpt.entries.len(),
        info.sector_total
    )));

    let mut images: Vec<ImageEntry> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    // The degraded capture cuts at boot_a: everything from it up is
    // skipped even though boot_a itself sits fully below the mask.
    let cut_lba = gpt
        .entries
        .iter()
        .find(|e| e.name == "boot_a")
        .map(|e| e.first_lba);
    for e in &gpt.entries {
        if stock_loader {
            // On the stock loader everything from GUARD_LBA up reads
            // back as 0xCC: a partition reaching past the mask (fully
            // above it or straddling it, like boot_b) is garbage for
            // the part the mask covers — skip it whole instead of
            // dumping 0xCC waste.
            let above_mask = e.first_lba + e.sectors > GUARD_LBA as u64;
            let past_boot_a = cut_lba.is_some_and(|lba| e.first_lba >= lba);
            if above_mask || past_boot_a {
                skipped.push(e.name.clone());
                let why = if past_boot_a && !above_mask {
                    "от boot_a и выше не снимается".to_string()
                } else {
                    "выше маски чтения (32 МиБ)".to_string()
                };
                progress(Progress::Step(format!(
                    "{name}: {why} — пропускаю",
                    name = e.name
                )));
                continue;
            }
        }
        let file_name = jb_core::backup_store::BackupStore::image_name(e.idx, &e.name);
        let path = out_dir.join(&file_name);
        progress(Progress::PartitionStart {
            name: e.name.clone(),
            sectors: e.sectors,
        });
        let mut out = std::fs::File::create(&path)?;
        let mut hasher = Sha256::new();
        // one reusable full-chunk buffer; the last chunk of a partition
        // is a prefix slice
        let mut chunk = vec![0u8; CHUNK_SECTORS as usize * 512];
        let mut done: u64 = 0;
        while done < e.sectors {
            let n = (e.sectors - done).min(CHUNK_SECTORS as u64) as usize;
            let lba = u32::try_from(e.first_lba + done).map_err(|_| {
                UsbError::Loader(format!("{}: LBA за пределами u32-протокола", e.name))
            })?;
            s.read_lba(lba, &mut chunk[..n * 512])?;
            // In usbplug mode a 0xCC first sector means the mask is
            // still active (the patch failed) — the whole dump would be
            // garbage. On the stock loader masked partitions never
            // reach this loop (skipped up front), so a 0xCC read here
            // is wrong under both personas.
            if !stock_loader && done == 0 && chunk[..512].iter().all(|&b| b == 0xcc) {
                let _ = std::fs::remove_file(&path);
                return Err(UsbError::WrongMode(format!(
                    "{}: весь 0xCC — маска активна, дамп негоден",
                    e.name
                )));
            }
            hasher.update(&chunk[..n * 512]);
            out.write_all(&chunk[..n * 512])?;
            done += n as u64;
            progress(Progress::PartitionProgress {
                name: e.name.clone(),
                done,
                total: e.sectors,
            });
        }
        out.flush()?;
        drop(out);
        let size = std::fs::metadata(&path)?.len();
        if size != e.sectors * 512 {
            return Err(UsbError::Loader(format!(
                "размер не сошёлся: {file_name} ({size} != {})",
                e.sectors * 512
            )));
        }
        // the checksum was streamed while dumping — no second pass;
        // everything dumped here sits fully below the stock read mask,
        // so it is intact
        images.push(ImageEntry {
            partition: e.name.clone(),
            file: file_name,
            sha256: hex::encode(hasher.finalize()),
            size,
            complete: true,
        });
        progress(Progress::PartitionDone {
            name: e.name.clone(),
        });
    }

    if !skipped.is_empty() {
        progress(Progress::Step(format!(
            "внимание: бэкап частичный — снимаются только разделы до boot_a \
             (от boot_a и выше 32 МиБ стоковый лоадер читает 0xCC), \
             не снимались: {}",
            skipped.join(", ")
        )));
    }

    progress(Progress::Step(format!(
        "готово: {} (дамп по GPT, не сырой NAND)",
        out_dir.display()
    )));
    Ok(BackupReport {
        out_dir: out_dir.to_path_buf(),
        partitions: gpt.entries.len(),
        sectors_total: info.sector_total as u64,
        read_limit_old: old_limit,
        images,
        skipped,
    })
}
