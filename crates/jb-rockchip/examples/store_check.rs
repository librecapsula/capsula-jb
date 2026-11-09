//! store_check — validate a backup directory without hardware: what
//! the v2 catalog decides and, if it fails, the exact reason. Run from
//! the repository root:
//!
//!   cargo run -p jb-rockchip --example store_check --release -- backup/capsule/<serial>

use jb_core::backup_catalog;
use std::path::Path;

fn main() {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: store_check <backup-dir>");
        std::process::exit(2);
    };
    let dir = Path::new(&dir);
    let Some(entry) = backup_catalog::read(dir) else {
        println!("НЕВАЛИДЕН: нет читаемого backup.json (нет бэкапа или старый формат)");
        std::process::exit(1);
    };
    println!(
        "open: ок, {}: {} ({}), образов: {}, метаданных: {}",
        entry.manifest.model.label(),
        entry.manifest.key,
        match entry.manifest.status {
            backup_catalog::Status::Complete => "полный",
            backup_catalog::Status::Partial => "с оговорками",
        },
        entry.manifest.images.len(),
        entry.manifest.extras.len(),
    );
    for note in &entry.manifest.notes {
        println!("  заметка: {note}");
    }
    match entry.verify() {
        Ok(()) => println!("ВАЛИДЕН: суммы полны и сходятся"),
        Err(e) => {
            println!("НЕВАЛИДЕН (verify): {e}");
            std::process::exit(1);
        }
    }
}
