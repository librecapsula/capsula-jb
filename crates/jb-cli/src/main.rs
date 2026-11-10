//! capsula-jb — the VK Capsula userspace jailbreak CLI: backup /
//! userdata-overlay / monitor. The data partition is not signed and is
//! freely rewritable — the overlay with adbd goes into it, everything
//! else stays untouched.

mod backend;
mod logging;
mod progress;
mod util;

use anyhow::bail;
use clap::{Parser, Subcommand};

use jb_core::format::humansize;
use jb_core::scenario::{Params, WritePlan};
use jb_core::session::{Command as SessionCommand, Decision, Event, Session};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "capsula-jb",
    version,
    about = "Джейлбрейк VK Capsula: дамп → оверлей adbd в userdata → ADB"
)]
struct Cli {
    /// The speaker model this run targets (capsula |
    /// capsula-mini-amlogic); required by the device commands
    #[arg(long, value_name = "MODEL", global = true)]
    model: Option<String>,

    /// How many seconds to wait for the speaker on USB (0 = forever)
    #[arg(long, value_name = "SEC", default_value = "0", global = true)]
    wait: u64,

    /// Do not ask for confirmation before writing userdata (for scripts)
    #[arg(long, global = true)]
    yes: bool,

    /// Build a clean image instead of patching the dump (contents are lost)
    #[arg(long, global = true)]
    format_userdata: bool,

    /// Skip backup verification and capture: the image is built from
    /// scratch (contents are lost; edge cases — e.g. a broken directory)
    #[arg(long, global = true)]
    no_backup: bool,

    /// Do not put adbd into the overlay (only the gadget flag and the
    /// jailbreak marker are written)
    #[arg(long, global = true)]
    no_adbd: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Only dump the partitions into backup/ (nothing is written to the speaker)
    Backup,
    /// Restore everything restorable from the verified backup (the
    /// full rollback: every intact partition image, plus the GPT and
    /// the idblock on rockchip)
    Restore,
    /// List the backups under backup/ — offline, no device needed
    /// (what the TUI shows in its backup pane)
    Backups {
        /// Verify every listed backup (a full SHA256 pass over the files)
        #[arg(long)]
        verify: bool,
    },
    /// Delete one backup directory — offline, no device needed
    /// (the TUI's "delete backup" button)
    #[command(name = "backup-delete")]
    BackupDelete {
        /// The model: capsula | capsula-mini-amlogic
        model: String,
        /// The column key: the serial (capsula) or the deviceid (capsula-mini-amlogic)
        key: String,
    },
    /// Live log of USB events
    Monitor,
}

fn main() -> anyhow::Result<()> {
    // No arguments at all AND an interactive terminal — the TUI (every
    // platform). Any argument, even a bare global flag, keeps the CLI
    // path below; a piped/redirected stdio (agents, CI) also falls
    // through — the CLI confirm flow is the agent-facing contract. The
    // TUI installs its own log bridge, so the CLI logger must not be
    // initialized on this branch.
    if std::env::args_os().nth(1).is_none() && interactive_terminal() {
        // An elevated relaunch (Windows UAC manifest) may start in
        // C:\Windows\System32; the tool writes backup/work relative to
        // the CWD, so move next to the executable in that case. (The
        // log line is not shown: the bridge is installed inside
        // jb_tui::run, after this point.)
        let _ = win_drv::fix_cwd_if_system32();
        if let Err(e) = jb_tui::run(backend::for_model) {
            // the TUI is torn down; the detached log bridge prints
            // this with a timestamp
            log::error!("{e:?}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // The logger comes first: all output, including the final error,
    // goes through it.
    logging::init();
    if let Some(cwd) = win_drv::fix_cwd_if_system32() {
        log::info!("рабочий каталог: {}", cwd.display());
    }
    let cli = Cli::parse();
    let wait = if cli.wait == 0 {
        None
    } else {
        Some(Duration::from_secs(cli.wait))
    };
    // The scenario lives in an inner function: neither `?` nor `bail!`
    // inside it lets an error slip past the interception below
    // (otherwise the runtime prints «Error: …» without a timestamp).
    let result = run(cli, wait);
    if let Err(e) = &result {
        log::error!("{e:?}");
    }
    // Launched from a normal console, Windows elevates the tool into a
    // NEW console window (see the UAC manifest note in .cargo/config.toml);
    // keep that window open so the output is readable. A shared terminal
    // or a redirected/none console gets no pause.
    win_drv::pause_if_own_console();
    match result {
        Ok(()) => Ok(()),
        // The error is already logged with a timestamp; exit silently
        // with code 1 so the runtime does not print it a second time
        // («Error: …»).
        Err(_) => std::process::exit(1),
    }
}

/// Parse the subcommand and run the scenario; all errors go through
/// the return value (intercepted in main and logged with a timestamp).
/// The scenario runs on the session worker thread; this loop is the
/// UI side: progress bars, confirmations, the final result.
fn run(cli: Cli, wait: Option<Duration>) -> anyhow::Result<()> {
    let p = progress::CliProgress::new();
    // The offline commands do not wait for a device and do not build a
    // driver session.
    match &cli.command {
        Some(Command::Monitor) => return monitor_bus(),
        Some(Command::Backups { verify }) => return list_backups(*verify),
        Some(Command::BackupDelete { model, key }) => return delete_backup(model, key, cli.yes),
        _ => {}
    }
    // The model comes first: the session worker polls only its backend,
    // so a device of the other platform on the bus is not even seen.
    let model = match cli.model.as_deref() {
        Some(name) => jb_core::session::Model::from_name(name).ok_or_else(|| {
            anyhow::anyhow!("неизвестная модель {name:?} (capsula | capsula-mini-amlogic)")
        })?,
        None => bail!("укажите модель: --model capsula | capsula-mini-amlogic"),
    };
    let session = Session::spawn(vec![backend::for_model(model)], wait);
    session.command(match &cli.command {
        None => SessionCommand::Jailbreak(Params {
            format_userdata: cli.format_userdata,
            no_backup: cli.no_backup,
            adbd: !cli.no_adbd,
        }),
        Some(Command::Backup) => {
            if cli.no_backup {
                bail!("--no-backup несовместим с подкомандой backup");
            }
            SessionCommand::Backup
        }
        Some(Command::Restore) => {
            if cli.no_backup {
                bail!("--no-backup несовместим с подкомандой restore");
            }
            SessionCommand::Restore
        }
        // Handled above (the offline commands return before the
        // session).
        Some(Command::Monitor) => unreachable!("monitor handled above"),
        Some(Command::Backups { .. }) | Some(Command::BackupDelete { .. }) => {
            unreachable!("offline commands handled above")
        }
    });
    // A refused confirmation with its own actionable error (the
    // non-interactive stdin message): substituted for the worker's
    // generic "прервано пользователем" below.
    let mut abort_reason: Option<anyhow::Error> = None;
    loop {
        match session.events().recv() {
            Ok(Event::WaitingForDevice { hints }) => {
                for h in hints {
                    log::info!("{h}");
                }
            }
            Ok(Event::DeviceConnected { identity }) => {
                log::info!("колонка: {} ({})", identity.key, identity.model.label())
            }
            // The platform logs the backup details inside
            // ensure_backup; nothing to add here.
            Ok(Event::BackupState(_)) => {}
            Ok(Event::Progress(ev)) => p.handle(ev),
            Ok(Event::ConfirmRequested { plan, reply }) => match confirm_write(&plan, cli.yes) {
                Ok(()) => {
                    let _ = reply.send(Decision::Proceed);
                }
                Err(e) => {
                    abort_reason = Some(e);
                    let _ = reply.send(Decision::Abort);
                }
            },
            Ok(Event::Finished(result)) => {
                return match (result, abort_reason) {
                    (Err(e), Some(reason)) if e.to_string() == "прервано пользователем" => {
                        Err(reason)
                    }
                    (r, _) => r,
                };
            }
            Err(_) => bail!("поток сессии завершился неожиданно"),
        }
    }
}

/// Write confirmation (skipped with --yes). The scenario requests it
/// after measuring the partition and BEFORE any writing action — a
/// refusal is safe. One wording for all platforms: the partition name,
/// size and mode description come from the platform's WritePlan.
fn confirm_write(plan: &WritePlan, yes: bool) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    let verb = if plan.restore {
        "восстановление"
    } else {
        "запись"
    };
    log::info!(
        "{verb} {} ({}): {}",
        plan.partition,
        humansize(plan.size),
        plan.mode
    );
    log::info!("продолжить? [y/N]");
    ask_yes("записи", "ничего не записано")
}

/// Both stdin and stdout are terminals — a human at a console (the
/// TUI gate). Anything piped/redirected (an agent shell, CI) is NOT
/// interactive, whatever the controlling terminal is.
fn interactive_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The [y/N] prompt. Non-interactive stdin (CI, LLM agent shells,
/// redirected input): never hang on a prompt nobody can answer — fail
/// with an actionable message instead. Nothing has been done at this
/// point, so an exit is safe.
fn ask_yes(what: &str, not_done: &str) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    std::io::stdout().flush().ok();
    if !std::io::stdin().is_terminal() {
        bail!(
            "нужно подтверждение {what}, но stdin не интерактивен; {not_done}. \
             Согласуйте с пользователем и перезапустите с --yes"
        );
    }
    let mut ans = String::new();
    std::io::stdin().read_line(&mut ans)?;
    let ans = ans.trim().to_lowercase();
    if ans != "y" && ans != "yes" {
        bail!("прервано пользователем");
    }
    Ok(())
}

/// Live log of USB events from both vendors: the rockchip monitor
/// (2207:*) plus tracking the appearance/disappearance of 1b8e:c003.
fn monitor_bus() -> anyhow::Result<()> {
    log::info!("Монитор USB (2207:* rockchip, 1b8e:c003 amlogic), Ctrl-C — выход.");
    let mut m = rockusb::DeviceMonitor::new();
    let mut amlogic_on_bus = ::amlogic::present().unwrap_or(false);
    m.poll()?;
    if let Some(best) = rockusb::transport::DeviceMode::best(m.modes()) {
        log::info!("сейчас на шине: 2207:{:04x} ({})", best.pid(), best.label());
    }
    if amlogic_on_bus {
        log::info!("сейчас на шине: 1b8e:c003 (amlogic download)");
    }
    loop {
        // 2207:* events are logged by rockusb::DeviceMonitor itself (usb:
        // appeared/gone); here we only watch the amlogic gadget.
        m.poll()?;
        let now = ::amlogic::present().unwrap_or(amlogic_on_bus);
        if now != amlogic_on_bus {
            if now {
                log::info!("появился 1b8e:c003 (amlogic download)");
            } else {
                log::info!("пропал   1b8e:c003 (amlogic download)");
            }
            amlogic_on_bus = now;
        }
        std::thread::sleep(rockusb::monitor::POLL_INTERVAL);
    }
}

/// `capsula-jb backups` — the offline backup listing: everything the
/// TUI's backup pane shows, for agents and scripts. `--verify` adds
/// the full SHA256 pass over every listed directory.
fn list_backups(verify: bool) -> anyhow::Result<()> {
    let entries = jb_core::backup_catalog::scan(std::path::Path::new("backup"));
    if entries.is_empty() {
        log::info!("бэкапов нет (каталог backup/ пуст или отсутствует)");
        return Ok(());
    }
    for e in &entries {
        let status = match e.manifest.status {
            jb_core::backup_catalog::Status::Complete => "полный",
            jb_core::backup_catalog::Status::Partial => "с оговорками",
        };
        log::info!(
            "{} {}: {} · {} · {}",
            e.manifest.model.label(),
            e.manifest.key,
            e.manifest.created_utc,
            humansize(e.total_bytes()),
            status,
        );
        log::info!("  каталог: {}", e.dir.display());
        for note in &e.manifest.notes {
            log::info!("  заметка: {note}");
        }
        if verify {
            match e.verify() {
                Ok(()) => log::info!("  проверка: ок"),
                Err(err) => log::error!("  проверка: НЕ сошлась: {err}"),
            }
        }
    }
    Ok(())
}

/// `capsula-jb backup-delete <model> <key>` — the offline twin of the
/// TUI's "delete backup" button. The confirmation follows the write
/// rules: interactive [y/N], non-interactive (agents) refuses and
/// points at --yes.
fn delete_backup(model: &str, key: &str, yes: bool) -> anyhow::Result<()> {
    let model = jb_core::session::Model::from_name(model).ok_or_else(|| {
        anyhow::anyhow!("неизвестная модель {model:?} (capsula | capsula-mini-amlogic)")
    })?;
    let Some(entry) = jb_core::backup_catalog::open(std::path::Path::new("backup"), model, key)
    else {
        bail!("бэкап не найден: backup/{}/{key}", model.dir_name());
    };
    log::info!(
        "удалить бэкап {} · {} · {}?",
        entry.dir.display(),
        model.label(),
        humansize(entry.total_bytes())
    );
    for note in &entry.manifest.notes {
        log::info!("  заметка: {note}");
    }
    log::info!("продолжить? [y/N]");
    if !yes {
        ask_yes("удаления", "бэкап не удалён")?;
    }
    let dir = entry.dir.clone();
    entry.delete()?;
    log::info!("удалён: {}", dir.display());
    Ok(())
}
