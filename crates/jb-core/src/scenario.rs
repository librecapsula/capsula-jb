//! The platform-agnostic jailbreak scenario. One skeleton drives the
//! flow — ensure the backup (session + capture) → measure the data
//! partition → confirm → build the image → write → finish — and a
//! platform fills in the device-specific steps through the [`Platform`]
//! trait. The session bring-up itself is the backend's
//! [`crate::backend::Backend::connect`]: the scenario starts with the
//! device already connected. The same skeleton also runs the
//! backup-only flow.
//!
//! Nothing is verified after the write on any platform (the write is
//! the last touching of the device), matching the established
//! rockchip flow.

use crate::overlay::{self, OverlayFile};
use crate::progress::ProgressFn;
use anyhow::{Result, bail};
use std::path::PathBuf;

/// Run modifiers, straight from the CLI flags / the TUI settings.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Build the image from scratch even when a verified base exists
    /// (--format-userdata): the partition content does not survive.
    pub format_userdata: bool,
    /// Skip both the check and the capture of the backup (--no-backup).
    pub no_backup: bool,
    /// Put adbd into the overlay (the default; --no-adbd turns it off —
    /// only the gadget flag and the jailbreak marker are written then).
    pub adbd: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            format_userdata: false,
            no_backup: false,
            adbd: true,
        }
    }
}

/// The backup situation of the CONNECTED column, as the local catalog
/// sees it (no device I/O). This is what a UI shows next to the
/// column; the real checksum verification still happens in
/// [`Platform::ensure_backup`] before a capture becomes the base of
/// the only copy.
#[derive(Debug, Clone)]
pub enum BackupState {
    /// No capture directory for this column at all.
    None,
    /// A capture that opened and passed the checksum verification —
    /// usable as the patch base and for restore.
    Verified { dir: PathBuf },
    /// A capture exists but cannot serve as a base: taken with caveats
    /// (the first-generation 0xCC mask, amlogic bad blocks) or failed
    /// verification. `why` carries the reasons for the UI.
    Unusable { dir: PathBuf, why: Vec<String> },
}

/// What [`Platform::ensure_backup`] delivers: the backup decision is
/// made (a new capture taken or an existing one reused).
pub struct Prepared {
    /// Verified capture of the data partition usable as the patch base.
    /// None = build from scratch (no capture, a capture that cannot
    /// serve as a base, or --no-backup).
    pub base: Option<PathBuf>,
    /// A new capture was taken this run (false = a verified capture for
    /// this column already existed, or none was taken).
    pub dumped: bool,
    /// The capture directory, for the backup-only report. None when no
    /// capture was considered (--no-backup).
    pub dir: Option<PathBuf>,
    /// The capture is fully intact; false = taken with caveats the
    /// platform warns about (the first-generation 0xCC mask, bad
    /// blocks...).
    pub complete: bool,
}

/// The write about to happen, as the confirmation prompt sees it. The
/// platform wording (filesystem, what survives) is baked into `mode`
/// by [`Platform::describe_write`].
#[derive(Clone, Debug)]
pub struct WritePlan {
    /// The partition name as the platform calls it (userdata on
    /// rockchip, data on amlogic).
    pub partition: &'static str,
    /// Image size in bytes — exactly the partition size: the image must
    /// cover the partition and fit it.
    pub size: u64,
    /// The image is built from scratch: the current partition content
    /// does not survive.
    pub fresh: bool,
    /// --no-backup run.
    pub no_backup: bool,
    /// The plan restores the verified capture instead of writing a
    /// patched image (the confirm line says "восстановление").
    pub restore: bool,
    /// The full human description of the write mode.
    pub mode: String,
}

/// Confirmation callback: log the prompt and ask; an error aborts the
/// run before anything is written. Skipped entirely by the caller when
/// --yes is given.
pub type Confirm<'a> = &'a dyn Fn(&WritePlan) -> Result<()>;

/// The device-specific steps of the scenario, implemented by the CLI
/// backends over their transports. Every hook may talk to the device
/// (and, on amlogic, go through reconnect windows) — the skeleton only
/// fixes the order. The device is already connected
/// ([`crate::backend::Backend::connect`]) when the hooks run.
pub trait Platform {
    /// Settle the backup: capture it (a checkpoint loop where the
    /// platform needs one) or find the verified capture of this column.
    /// `no_backup` skips both the check and the capture; the session
    /// the hooks run over is kept in the platform state.
    fn ensure_backup(&mut self, no_backup: bool, progress: ProgressFn) -> Result<Prepared>;

    /// The connected column's [`BackupState`] from the local catalog
    /// (pure filesystem, no device I/O).
    fn backup_state(&self) -> BackupState;

    /// Logical size of the data partition, bytes (the size the image
    /// must be built to).
    fn data_size(&mut self, progress: ProgressFn) -> Result<u64>;

    /// The partition name as the platform calls it (userdata on
    /// rockchip, data on amlogic) — for the confirmation line.
    fn data_partition(&self) -> &'static str;

    /// The write-mode description for the confirmation line.
    fn describe_write(&self, fresh: bool, no_backup: bool) -> String;

    /// The restore-mode description (writing the verified capture back
    /// over the current content) for the confirmation line.
    fn describe_restore(&self) -> String;

    /// Build the data partition image with the overlay files: EXACTLY
    /// `size` bytes, patching `base` when given (and `fresh` is false),
    /// from scratch otherwise.
    fn build_data_image(
        &self,
        base: Option<&std::path::Path>,
        out: &std::path::Path,
        size: u64,
        files: &[OverlayFile],
        progress: ProgressFn,
    ) -> Result<()>;

    /// Write the image built by [`Self::build_data_image`] to the
    /// device. The image is exactly the partition size; nothing is
    /// read back afterwards.
    fn write_data_image(&mut self, image: &std::path::Path, progress: ProgressFn) -> Result<()>;

    /// The full-restore set of the connected column: every INTACT
    /// partition image of the verified capture, with the partitions
    /// whose capture is garbage/short (the first-generation 0xCC mask,
    /// amlogic bad blocks) reported in `skipped` — those must never be
    /// flashed back. An Err means there is no verified capture at all.
    fn restore_set(&mut self) -> Result<(Vec<RestoreItem>, Vec<String>)>;

    /// Write the restore set back to the device — everything that can
    /// be restored (plus the restorable platform metadata: the GPT and
    /// the idblock on rockchip; the device-unique AML-reserved areas
    /// and the environment on amlogic are NEVER written). Called after
    /// the confirmation; nothing is verified afterwards.
    fn write_restore(&mut self, items: &[RestoreItem], progress: ProgressFn) -> Result<()>;

    /// Reboot the device and print the platform's final instructions.
    fn finish(&mut self, progress: ProgressFn) -> Result<()>;

    /// The backup-only flow's result line (platform wording: what was
    /// captured, where, and the caveats).
    fn report_backup(&self, prepared: &Prepared);
}

/// One image of a full restore.
#[derive(Debug, Clone)]
pub struct RestoreItem {
    pub partition: String,
    pub image: PathBuf,
    pub size: u64,
}

/// The jailbreak flow over any platform. The overlay file set (adbd +
/// the gadget flag + the jailbreak marker) is platform-independent and
/// built here.
pub fn jailbreak(
    platform: &mut dyn Platform,
    params: &Params,
    confirm: Confirm,
    progress: ProgressFn,
) -> Result<()> {
    let prepared = platform.ensure_backup(params.no_backup, progress)?;
    let size = platform.data_size(progress)?;
    let fresh = params.format_userdata || params.no_backup || prepared.base.is_none();
    let plan = WritePlan {
        partition: platform.data_partition(),
        size,
        fresh,
        no_backup: params.no_backup,
        restore: false,
        mode: platform.describe_write(fresh, params.no_backup),
    };
    // Nothing has been written at this point, so a refusal is safe.
    confirm(&plan)?;

    let base = if fresh {
        None
    } else {
        prepared.base.as_deref()
    };
    let files = overlay::overlay_files(overlay_marker(), params.adbd);
    let out = std::path::Path::new("work").join("userdata.img");
    platform.build_data_image(base, &out, size, &files, progress)?;
    platform.write_data_image(&out, progress)?;
    platform.finish(progress)?;
    Ok(())
}

/// The restore flow: write the verified capture of the data partition
/// back to the device — the exact inverse of the jailbreak's write
/// step. The overlay and everything written after the capture do not
/// survive; the capture's own content (session files included) does.
/// `--no-backup` is rejected by the CLI for this subcommand.
pub fn restore(platform: &mut dyn Platform, confirm: Confirm, progress: ProgressFn) -> Result<()> {
    let _prepared = platform.ensure_backup(false, progress)?;
    let (items, skipped) = platform.restore_set()?;
    if items.is_empty() {
        bail!(
            "восстановление невозможно: в захвате этой колонки нет ни \
             одного целого образа{}",
            if skipped.is_empty() {
                String::new()
            } else {
                format!(" (все нецелые: {})", skipped.join(", "))
            }
        );
    }
    // The per-image size sanity (the capture must fit the partition as
    // it is now — bad blocks may have grown it) is the platform's, in
    // write_restore: only it knows the live tables.
    let total: u64 = items.iter().map(|i| i.size).sum();
    let mut mode = platform.describe_restore();
    if !skipped.is_empty() {
        mode.push_str(&format!(
            "; пропущены (захват нецелый): {}",
            skipped.join(", ")
        ));
    }
    let plan = WritePlan {
        partition: "все разделы",
        size: total,
        fresh: false,
        no_backup: false,
        restore: true,
        mode,
    };
    // Nothing has been written at this point, so a refusal is safe.
    confirm(&plan)?;
    platform.write_restore(&items, progress)?;
    platform.finish(progress)?;
    Ok(())
}

/// The backup-only flow: ensure the backup (capture, never writes to
/// the column) and report. `--no-backup` is rejected by the CLI for
/// this subcommand, so `no_backup` is always false here.
pub fn backup(platform: &mut dyn Platform, progress: ProgressFn) -> Result<()> {
    let prepared = platform.ensure_backup(false, progress)?;
    platform.report_backup(&prepared);
    Ok(())
}

/// The jailbreak marker written into the overlay: the tool and the run
/// time, so a booted column can tell who unlocked it.
fn overlay_marker() -> String {
    format!("capsula-jb userspace {}\n", crate::format::utc_now_iso())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Progress;
    use std::cell::RefCell;
    use std::path::Path;

    /// A recording platform: every hook appends to `calls`; the
    /// configurable fields drive the skeleton's branches.
    struct MockPlatform {
        base: Option<PathBuf>,
        size: u64,
        calls: RefCell<Vec<&'static str>>,
        /// The base the build hook actually received.
        got_base: RefCell<Option<PathBuf>>,
        /// The image the write hook actually received.
        written_image: RefCell<Option<PathBuf>>,
        /// What restore_set returns: (items, skipped). An Err in
        /// `restore_err` short-circuits instead.
        restore_items: Vec<RestoreItem>,
        restore_skipped: Vec<String>,
        restore_err: bool,
        /// The items write_restore actually received.
        restored: RefCell<Vec<String>>,
    }

    impl MockPlatform {
        fn new(base: Option<PathBuf>) -> Self {
            MockPlatform {
                base,
                size: 4096,
                calls: RefCell::new(vec![]),
                got_base: RefCell::new(None),
                written_image: RefCell::new(None),
                restore_items: vec![RestoreItem {
                    partition: "testpart".into(),
                    image: PathBuf::from("/tmp/never/testpart.img"),
                    size: 4096,
                }],
                restore_skipped: vec![],
                restore_err: false,
                restored: RefCell::new(vec![]),
            }
        }

        fn mark(&self, what: &'static str) {
            self.calls.borrow_mut().push(what);
        }
    }

    impl Platform for MockPlatform {
        fn ensure_backup(&mut self, no_backup: bool, _progress: ProgressFn) -> Result<Prepared> {
            self.mark("prepare");
            let base = if no_backup { None } else { self.base.clone() };
            Ok(Prepared {
                base,
                dumped: true,
                dir: self
                    .base
                    .as_ref()
                    .map(|b| b.parent().unwrap().to_path_buf()),
                complete: true,
            })
        }

        fn backup_state(&self) -> BackupState {
            BackupState::None
        }

        fn data_size(&mut self, _progress: ProgressFn) -> Result<u64> {
            self.mark("data_size");
            Ok(self.size)
        }

        fn data_partition(&self) -> &'static str {
            "testpart"
        }

        fn describe_write(&self, fresh: bool, _no_backup: bool) -> String {
            if fresh { "fresh" } else { "patch" }.to_string()
        }

        fn describe_restore(&self) -> String {
            "restore".to_string()
        }

        fn build_data_image(
            &self,
            base: Option<&Path>,
            _out: &Path,
            size: u64,
            _files: &[OverlayFile],
            _progress: ProgressFn,
        ) -> Result<()> {
            self.mark("build");
            assert_eq!(size, self.size);
            *self.got_base.borrow_mut() = base.map(|p| p.to_path_buf());
            Ok(())
        }

        fn write_data_image(&mut self, image: &Path, _progress: ProgressFn) -> Result<()> {
            self.mark("write");
            *self.written_image.borrow_mut() = Some(image.to_path_buf());
            Ok(())
        }

        fn restore_set(&mut self) -> Result<(Vec<RestoreItem>, Vec<String>)> {
            self.mark("set");
            if self.restore_err {
                bail!("восстановление невозможно: нет верифицированного захвата");
            }
            Ok((self.restore_items.clone(), self.restore_skipped.clone()))
        }

        fn write_restore(&mut self, items: &[RestoreItem], _progress: ProgressFn) -> Result<()> {
            self.mark("restore_write");
            *self.restored.borrow_mut() = items.iter().map(|i| i.partition.clone()).collect();
            Ok(())
        }

        fn finish(&mut self, _progress: ProgressFn) -> Result<()> {
            self.mark("finish");
            Ok(())
        }

        fn report_backup(&self, _prepared: &Prepared) {
            self.mark("report");
        }
    }

    fn noop(_ev: Progress) {}

    #[test]
    fn jailbreak_runs_hooks_in_order_and_confirms_before_any_write() {
        let base = PathBuf::from("/tmp/never/userdata.img");
        let mut p = MockPlatform::new(Some(base.clone()));
        jailbreak(
            &mut p,
            &Params::default(),
            &|plan| {
                assert_eq!(
                    (plan.size, plan.fresh, plan.no_backup),
                    (4096, false, false)
                );
                assert_eq!(plan.mode, "patch");
                Ok(())
            },
            &noop,
        )
        .unwrap();
        assert_eq!(
            *p.calls.borrow(),
            vec!["prepare", "data_size", "build", "write", "finish"]
        );
        assert_eq!(p.got_base.borrow().as_deref(), Some(base.as_path()));
    }

    #[test]
    fn jailbreak_builds_fresh_when_the_base_is_missing_or_disabled() {
        // no base at all
        let mut p = MockPlatform::new(None);
        jailbreak(&mut p, &Params::default(), &|_| Ok(()), &noop).unwrap();
        assert!(p.got_base.borrow().is_none());
        assert_eq!(
            *p.calls.borrow(),
            vec!["prepare", "data_size", "build", "write", "finish"]
        );

        // a base exists but --no-backup drops it
        let mut p = MockPlatform::new(Some(PathBuf::from("/tmp/never/u.img")));
        jailbreak(
            &mut p,
            &Params {
                no_backup: true,
                ..Default::default()
            },
            &|plan| {
                assert!(plan.fresh && plan.no_backup);
                Ok(())
            },
            &noop,
        )
        .unwrap();
        assert!(p.got_base.borrow().is_none());
    }

    #[test]
    fn jailbreak_stops_when_the_confirmation_refuses() {
        let mut p = MockPlatform::new(None);
        let err = jailbreak(
            &mut p,
            &Params::default(),
            &|_| anyhow::bail!("прервано"),
            &noop,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "прервано");
        // nothing was built or written
        assert_eq!(*p.calls.borrow(), vec!["prepare", "data_size"]);
    }

    #[test]
    fn restore_writes_the_whole_intact_set() {
        let mut p = MockPlatform::new(None);
        p.restore_items = vec![
            RestoreItem {
                partition: "uboot".into(),
                image: PathBuf::from("/tmp/never/00_uboot.img"),
                size: 1024,
            },
            RestoreItem {
                partition: "testpart".into(),
                image: PathBuf::from("/tmp/never/01_testpart.img"),
                size: 4096,
            },
        ];
        p.restore_skipped = vec!["boot_b".into()];
        let confirmed = std::cell::RefCell::new(None);
        restore(
            &mut p,
            &|plan| {
                assert!(plan.restore);
                assert_eq!(plan.partition, "все разделы");
                assert_eq!(plan.size, 1024 + 4096);
                assert_eq!(plan.mode, "restore; пропущены (захват нецелый): boot_b");
                *confirmed.borrow_mut() = Some(plan.mode.clone());
                Ok(())
            },
            &noop,
        )
        .unwrap();
        assert_eq!(
            confirmed.borrow().as_deref(),
            Some("restore; пропущены (захват нецелый): boot_b")
        );
        assert_eq!(
            *p.calls.borrow(),
            vec!["prepare", "set", "restore_write", "finish"]
        );
        assert_eq!(*p.restored.borrow(), vec!["uboot", "testpart"]);
    }

    #[test]
    fn restore_without_a_capture_refuses() {
        let mut p = MockPlatform::new(None);
        p.restore_err = true;
        let err = restore(&mut p, &|_| Ok(()), &noop).unwrap_err();
        assert!(
            err.to_string().contains("восстановление невозможно"),
            "{err}"
        );
        // nothing was written
        assert_eq!(*p.calls.borrow(), vec!["prepare", "set"]);
    }

    #[test]
    fn restore_with_no_intact_images_refuses() {
        let mut p = MockPlatform::new(None);
        p.restore_items = vec![];
        p.restore_skipped = vec!["userdata".into(), "boot_b".into()];
        let err = restore(&mut p, &|_| Ok(()), &noop).unwrap_err();
        assert!(
            err.to_string().contains("нет ни одного целого образа"),
            "{err}"
        );
        assert_eq!(*p.calls.borrow(), vec!["prepare", "set"]);
    }

    #[test]
    fn restore_stops_when_the_confirmation_refuses() {
        let mut p = MockPlatform::new(None);
        let err = restore(&mut p, &|_| anyhow::bail!("прервано"), &noop).unwrap_err();
        assert_eq!(err.to_string(), "прервано");
        assert_eq!(*p.calls.borrow(), vec!["prepare", "set"]);
    }

    #[test]
    fn backup_reports_and_never_writes() {
        let mut p = MockPlatform::new(Some(PathBuf::from("/tmp/never/cap/userdata.img")));
        backup(&mut p, &noop).unwrap();
        assert_eq!(*p.calls.borrow(), vec!["prepare", "report"]);
    }
}
