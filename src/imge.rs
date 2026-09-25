//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use anyhow::{Result, anyhow};
use std::alloc::{Layout, alloc, dealloc};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const BLOCK_SIZE: usize = 1024 * 1024;

pub struct Drive {
    pub name: OsString,
    pub model: String,
    pub serial: String,
    pub is_removable: bool,
    pub is_mounted: bool,
    pub size: u64,
}

#[derive(PartialEq)]
pub enum VolumeType {
    Image,
    Drive,
}

#[derive(Copy, Clone, Default, PartialEq)]
pub enum Compression {
    #[default]
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

pub struct Volume {
    pub vtype: VolumeType,
    pub path: OsString,
    pub size: Option<u64>,
    pub compression: Compression,
}

#[derive(Default)]
pub struct Progress {
    pub size: u64,
    pub done: u64,
    pub copy_secs: u64,
    pub verify_secs: u64,
    pub finished: bool,
}

impl Progress {
    pub fn percents(&self) -> f64 {
        if self.size == 0 {
            0.0
        } else {
            (self.done as f64 / self.size as f64).min(1.0)
        }
    }
}

pub type ProgressMutex = Arc<Mutex<Progress>>;

pub fn list_drives(all_drives: bool) -> Result<Vec<Drive>> {
    let mut drives = Vec::new();

    for device in drives::get_devices()? {
        let mut is_mounted = false;

        for partition in device.partitions {
            if partition.mountpoint.is_some() {
                is_mounted = true;
                break;
            }
        }

        if device.is_removable || all_drives {
            drives.push(Drive {
                name: OsString::from(format!("/dev/{}", device.name)),
                model: device.model.unwrap_or_default(),
                serial: device.serial.unwrap_or_default(),
                is_removable: device.is_removable,
                is_mounted,
                size: device.size.get_raw_size() * 512,
            });
        }
    }

    drives.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(drives)
}

fn open_for_reading(vol: &Volume) -> Result<Box<dyn Read>> {
    let file = File::open(&vol.path)?;

    let file: Box<dyn Read> = match vol.compression {
        Compression::None => Box::new(file),
        Compression::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
        Compression::Bzip2 => Box::new(bzip2::read::BzDecoder::new(file)),
        Compression::Xz => Box::new(xz2::read::XzDecoder::new(file)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(file)?),
    };

    Ok(file)
}

// Encoders write their trailer on drop and ignore errors there, so they must be finished
// explicitly before the output is reported as complete.
trait FinishWrite: Write {
    fn finish(self: Box<Self>) -> io::Result<()>;
}

impl FinishWrite for File {
    fn finish(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}

macro_rules! impl_finish_write {
    ($($t:ty),*) => {$(
        impl FinishWrite for $t {
            fn finish(self: Box<Self>) -> io::Result<()> {
                <$t>::finish(*self).map(drop)
            }
        }
    )*};
}

impl_finish_write!(
    flate2::write::GzEncoder<File>,
    bzip2::write::BzEncoder<File>,
    xz2::write::XzEncoder<File>,
    zstd::stream::write::Encoder<'static, File>
);

fn open_for_writing(vol: &Volume) -> Result<Box<dyn FinishWrite>> {
    let mut options = OpenOptions::new();
    let mut options = options.create(true).write(true).truncate(true);
    if vol.vtype == VolumeType::Drive {
        options = options.custom_flags(libc::O_DSYNC)
    }
    let file = options.open(&vol.path)?;

    let file: Box<dyn FinishWrite> = match vol.compression {
        Compression::None => Box::new(file),
        Compression::Gzip => Box::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::default(),
        )),
        Compression::Bzip2 => Box::new(bzip2::write::BzEncoder::new(
            file,
            bzip2::Compression::default(),
        )),
        Compression::Xz => Box::new(xz2::write::XzEncoder::new(file, 3)),
        Compression::Zstd => Box::new(zstd::stream::write::Encoder::new(
            file,
            zstd::DEFAULT_COMPRESSION_LEVEL,
        )?),
    };

    Ok(file)
}

pub fn copy(
    src: &Volume,
    dest: &Volume,
    progress_mutex: &ProgressMutex,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    if src.vtype == VolumeType::Image
        && src.size.is_some()
        && dest.size.is_some()
        && src.size > dest.size
    {
        return Err(anyhow!(io::Error::from_raw_os_error(libc::EFBIG)));
    }

    let mut srcfile = open_for_reading(src)?;
    let mut destfile = open_for_writing(dest)?;
    let mut buffer = [0u8; BLOCK_SIZE];
    let size = src.size.unwrap_or_default();
    let mut done = 0;
    let timer = Instant::now();

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }

        // A char-device source never reaches EOF, so stop exactly at the known size.
        let want = if size > 0 {
            (size - done).min(BLOCK_SIZE as u64) as usize
        } else {
            BLOCK_SIZE
        };
        if want == 0 {
            break;
        }

        let len = srcfile.read(&mut buffer[..want])?;
        if len == 0 {
            break;
        }

        destfile.write_all(&buffer[..len])?;
        done += len as u64;
        progress_mutex.lock().unwrap().done = done;
    }

    destfile.finish()?;

    let mut progress = progress_mutex.lock().unwrap();
    progress.copy_secs = timer.elapsed().as_secs();
    progress.finished = true;

    Ok(())
}

struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuf {
    fn new(size: usize, align: usize) -> Result<Self> {
        let layout = Layout::from_size_align(size, align)?;
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(anyhow!("failed to allocate aligned buffer"));
        }
        Ok(Self { ptr, layout })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

// Like read_exact, but a short count at EOF is returned instead of being an error.
fn read_full(file: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

pub fn verify(
    image: &Volume,
    drive: &Volume,
    progress_mutex: &ProgressMutex,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let mut image_file = open_for_reading(image)?;
    let mut drive_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(&drive.path)?;

    let mut image_buffer = [0u8; BLOCK_SIZE];
    let mut drive_buf = AlignedBuf::new(BLOCK_SIZE, 4096)?;

    let timer = Instant::now();

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }

        let len = read_full(&mut image_file, &mut image_buffer)?;

        if len == 0 {
            break;
        }

        let drive_len = read_full(&mut drive_file, drive_buf.as_mut_slice())?;

        if drive_len < len || image_buffer[..len] != drive_buf.as_slice()[..len] {
            return Err(anyhow!(io::Error::other("Verification failed")));
        }

        let mut progress = progress_mutex.lock().unwrap();
        progress.done += len as u64;
    }

    let mut progress = progress_mutex.lock().unwrap();
    progress.verify_secs = timer.elapsed().as_secs();
    progress.finished = true;

    Ok(())
}

pub fn humanize(size: u64) -> String {
    let sfx = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB"];
    let mut s = size;
    let mut f = 0;
    let mut i = 0;

    while s >= 1024 && i < sfx.len() - 1 {
        f = s % 1024;
        s /= 1024;
        i += 1;
    }

    if i == 0 {
        format!("{} {}", s, sfx[0])
    } else {
        format!("{:.1} {}", s as f64 + f as f64 / 1024.0, sfx[i])
    }
}
