//! Flashing a partition back to the device. The only operation of the
//! tool that writes to flash: userdata (unsigned, freely rewritable).
//! Partition addresses come from the device GPT.

use crate::backup::CHUNK_SECTORS;
use crate::gpt::Gpt;
use jb_core::progress::{Progress, ProgressFn};
use rockusb::loader::LoaderSession;
use rockusb::{Error as UsbError, Transport};
use std::io::Read;
use std::path::Path;

/// Write an image into a partition: the image is zero-padded up to the
/// partition size. An image larger than the partition is an error.
/// The write is chunked so progress reporting works during the transfer.
pub fn write_partition_padded<T: Transport>(
    s: &mut LoaderSession<T>,
    gpt: &Gpt,
    part: &str,
    image: &Path,
    progress: ProgressFn,
) -> rockusb::Result<()> {
    let e = gpt
        .find(part)
        .ok_or_else(|| UsbError::Loader(format!("в GPT колонки нет раздела {part}")))?;
    let psize = e.sectors * 512;
    let fsize = std::fs::metadata(image)
        .map_err(|err| UsbError::Loader(format!("{}: {err}", image.display())))?
        .len();
    if fsize > psize {
        return Err(UsbError::Loader(format!(
            "{part}: образ {} больше раздела ({psize} байт)",
            image.display()
        )));
    }
    let psize = usize::try_from(psize)
        .map_err(|_| UsbError::Loader(format!("{part}: раздел не влезает в память")))?;
    progress(Progress::PartitionStart {
        name: part.to_string(),
        sectors: e.sectors,
    });
    // Stream the image into the head of the padded buffer: no second
    // full-size copy of the file, and no size TOCTOU between metadata
    // and read (a truncated file fails cleanly with UnexpectedEof).
    let mut data = vec![0u8; psize];
    let mut f = std::fs::File::open(image)?;
    f.read_exact(&mut data[..fsize as usize])?;

    let mut done: u64 = 0;
    while done < e.sectors {
        let n = (e.sectors - done).min(CHUNK_SECTORS as u64) as usize;
        let lba = u32::try_from(e.first_lba + done)
            .map_err(|_| UsbError::Loader(format!("{part}: LBA за пределами u32-протокола")))?;
        let off = done as usize * 512;
        s.write_lba(lba, &data[off..off + n * 512])?;
        done += n as u64;
        progress(Progress::PartitionProgress {
            name: part.to_string(),
            done,
            total: e.sectors,
        });
    }

    progress(Progress::PartitionDone {
        name: part.to_string(),
    });
    Ok(())
}

/// Write raw sectors at a fixed LBA — the restore path for what is
/// not a GPT partition: the table itself (LBA 1, 34 sectors) and the
/// idblock (LBA 0x40). `name` labels the progress lines. Chunked like
/// a partition write.
pub fn write_lba_image<T: Transport>(
    s: &mut LoaderSession<T>,
    lba: u32,
    name: &str,
    data: &[u8],
    progress: ProgressFn,
) -> rockusb::Result<()> {
    if data.is_empty() || !data.len().is_multiple_of(512) {
        return Err(UsbError::Loader(format!(
            "{name}: размер не кратен сектору ({})",
            data.len()
        )));
    }
    let sectors = (data.len() / 512) as u64;
    progress(Progress::PartitionStart {
        name: name.to_string(),
        sectors,
    });
    let mut done: u64 = 0;
    while done < sectors {
        let n = (sectors - done).min(CHUNK_SECTORS as u64) as usize;
        let off = done as usize * 512;
        s.write_lba(
            lba.checked_add(done as u32)
                .ok_or_else(|| UsbError::Loader(format!("{name}: LBA за пределами u32")))?,
            &data[off..off + n * 512],
        )?;
        done += n as u64;
        progress(Progress::PartitionProgress {
            name: name.to_string(),
            done,
            total: sectors,
        });
    }
    progress(Progress::PartitionDone {
        name: name.to_string(),
    });
    Ok(())
}
