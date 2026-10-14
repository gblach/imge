//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use anyhow::{Context, Result, anyhow, bail};
use sha2::digest::DynDigest;
use std::alloc::{Layout, alloc, dealloc};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

#[derive(Clone, Copy)]
enum ChecksumType {
    Sha1,
    Sha256,
}

impl ChecksumType {
    fn hasher(self) -> Box<dyn DynDigest> {
        match self {
            ChecksumType::Sha1 => Box::new(sha1::Sha1::default()),
            ChecksumType::Sha256 => Box::new(sha2::Sha256::default()),
        }
    }
}

#[derive(Clone)]
struct BmapRange {
    first: u64,
    last: u64,
    checksum: Option<String>,
}

#[derive(Clone)]
pub struct Bmap {
    pub image_size: u64,
    block_size: u64,
    checksum_type: ChecksumType,
    ranges: Vec<BmapRange>,
}

impl Bmap {
    pub fn parse(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let doc = roxmltree::Document::parse(&text)?;
        let root = doc.root_element();

        let version = root.attribute("version").unwrap_or_default();
        if !matches!(version.split('.').next(), Some("1" | "2")) {
            bail!("unsupported bmap version {version}");
        }

        let element = |name| root.children().find(|n| n.has_tag_name(name));
        let number = |name| -> Result<u64> {
            let text = element(name).and_then(|n| n.text()).unwrap_or_default();
            text.trim()
                .parse()
                .with_context(|| format!("invalid {name}"))
        };

        // Format 1.x has only SHA-1 checksums, stored in a differently named attribute.
        let checksum_type = element("ChecksumType")
            .and_then(|n| n.text())
            .map(str::trim);
        let (checksum_type, checksum_attr) = match checksum_type {
            None => (ChecksumType::Sha1, "sha1"),
            Some("sha1") => (ChecksumType::Sha1, "chksum"),
            Some("sha256") => (ChecksumType::Sha256, "chksum"),
            Some(other) => bail!("unsupported checksum type {other}"),
        };

        let mut bmap = Bmap {
            image_size: number("ImageSize")?,
            block_size: number("BlockSize")?,
            checksum_type,
            ranges: Vec::new(),
        };

        let block_map = element("BlockMap").context("missing BlockMap")?;
        let mut next_free = 0;
        for node in block_map.children().filter(|n| n.has_tag_name("Range")) {
            let text = node.text().unwrap_or_default().trim();
            let (first, last) = text.split_once('-').unwrap_or((text, text));
            let range = BmapRange {
                first: first
                    .trim()
                    .parse()
                    .with_context(|| format!("invalid range {text}"))?,
                last: last
                    .trim()
                    .parse()
                    .with_context(|| format!("invalid range {text}"))?,
                checksum: node.attribute(checksum_attr).map(str::to_lowercase),
            };

            // Copying reads the image as a stream, so ranges must be sorted and disjoint.
            if range.first < next_free
                || range.first > range.last
                || range.first * bmap.block_size >= bmap.image_size
            {
                bail!("invalid range {text}");
            }
            next_free = range.last + 1;
            bmap.ranges.push(range);
        }

        Ok(bmap)
    }

    fn byte_range(&self, range: &BmapRange) -> (u64, u64) {
        let start = range.first * self.block_size;
        let end = ((range.last + 1) * self.block_size).min(self.image_size);
        (start, end)
    }

    pub fn mapped_size(&self) -> u64 {
        self.ranges
            .iter()
            .map(|r| {
                let (start, end) = self.byte_range(r);
                end - start
            })
            .sum()
    }
}

pub struct Volume {
    pub vtype: VolumeType,
    pub path: OsString,
    pub size: Option<u64>,
    pub compression: Compression,
    pub bmap: Option<Bmap>,
}

#[derive(Default)]
pub struct Progress {
    pub size: u64,
    pub done: u64,
    // The decompressed size of a compressed image is unknown, so its progress is estimated
    // from how much of the compressed file has been read.
    pub compressed_size: u64,
    pub compressed_done: u64,
    pub copy_secs: f64,
    pub verify_secs: f64,
    pub finished: bool,
}

impl Progress {
    pub fn percents(&self) -> f64 {
        if self.size > 0 {
            (self.done as f64 / self.size as f64).min(1.0)
        } else if self.compressed_size > 0 {
            (self.compressed_done as f64 / self.compressed_size as f64).min(1.0)
        } else {
            0.0
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

struct CountingReader<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.inner.read(buf)?;
        self.count.fetch_add(len as u64, Ordering::Relaxed);
        Ok(len)
    }
}

fn decompress(file: impl Read + 'static, compression: Compression) -> Result<Box<dyn Read>> {
    let file: Box<dyn Read> = match compression {
        Compression::None => Box::new(file),
        Compression::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
        Compression::Bzip2 => Box::new(bzip2::read::BzDecoder::new(file)),
        Compression::Xz => Box::new(xz2::read::XzDecoder::new(file)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(file)?),
    };

    Ok(file)
}

fn open_for_reading(vol: &Volume) -> Result<Box<dyn Read>> {
    decompress(File::open(&vol.path)?, vol.compression)
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

// Reads and drops len bytes, since a decompressed stream cannot seek.
fn discard(
    file: &mut dyn Read,
    mut len: u64,
    buffer: &mut [u8],
    cancelled: &Arc<AtomicBool>,
) -> io::Result<()> {
    while len > 0 && !cancelled.load(Ordering::Relaxed) {
        let want = len.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..want])?;
        len -= want as u64;
    }
    Ok(())
}

fn copy_mapped(
    src: &Volume,
    dest: &Volume,
    bmap: &Bmap,
    progress_mutex: &ProgressMutex,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let mut srcfile = open_for_reading(src)?;
    let destfile = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_DSYNC)
        .open(&dest.path)?;
    let mut buffer = vec![0u8; BLOCK_SIZE];
    let mut hasher = bmap.checksum_type.hasher();
    let mut pos = 0;
    let mut done = 0;
    let timer = Instant::now();

    for range in &bmap.ranges {
        let (start, end) = bmap.byte_range(range);
        discard(&mut srcfile, start - pos, &mut buffer, cancelled)?;
        pos = start;

        while pos < end {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }

            let len = (end - pos).min(BLOCK_SIZE as u64) as usize;
            srcfile.read_exact(&mut buffer[..len])?;
            hasher.update(&buffer[..len]);
            destfile.write_all_at(&buffer[..len], pos)?;
            pos += len as u64;
            done += len as u64;
            progress_mutex.lock().unwrap().done = done;
        }

        let checksum: String = hasher
            .finalize_reset()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if range.checksum.as_ref().is_some_and(|c| *c != checksum) {
            bail!(
                "Checksum mismatch in blocks {}-{} of the image",
                range.first,
                range.last
            );
        }
    }

    let mut progress = progress_mutex.lock().unwrap();
    progress.copy_secs = timer.elapsed().as_secs_f64();
    progress.finished = true;

    Ok(())
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

    if let Some(bmap) = &src.bmap {
        return copy_mapped(src, dest, bmap, progress_mutex, cancelled);
    }

    let compressed_done = Arc::new(AtomicU64::new(0));
    let counting_file = CountingReader {
        inner: File::open(&src.path)?,
        count: compressed_done.clone(),
    };
    let mut srcfile = decompress(counting_file, src.compression)?;
    let mut destfile = open_for_writing(dest)?;
    let mut buffer = vec![0u8; BLOCK_SIZE];
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
        let mut progress = progress_mutex.lock().unwrap();
        progress.done = done;
        progress.compressed_done = compressed_done.load(Ordering::Relaxed);
    }

    destfile.finish()?;

    let mut progress = progress_mutex.lock().unwrap();
    progress.copy_secs = timer.elapsed().as_secs_f64();
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

fn read_full_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match file.read_at(&mut buf[total..], offset + total as u64) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn verify_mapped(
    image: &Volume,
    drive: &Volume,
    bmap: &Bmap,
    progress_mutex: &ProgressMutex,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let mut image_file = open_for_reading(image)?;
    let drive_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(&drive.path)?;

    let mut image_buffer = vec![0u8; BLOCK_SIZE];
    let mut drive_buf = AlignedBuf::new(BLOCK_SIZE, 4096)?;
    let mut pos = 0;

    let timer = Instant::now();

    for range in &bmap.ranges {
        let (start, end) = bmap.byte_range(range);
        discard(&mut image_file, start - pos, &mut image_buffer, cancelled)?;
        pos = start;

        while pos < end {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }

            let len = (end - pos).min(BLOCK_SIZE as u64) as usize;
            image_file.read_exact(&mut image_buffer[..len])?;

            // O_DIRECT needs the read length aligned too, so read a bit past the range end.
            let drive_slice = &mut drive_buf.as_mut_slice()[..len.next_multiple_of(4096)];
            let drive_len = read_full_at(&drive_file, drive_slice, pos)?;

            if drive_len < len || image_buffer[..len] != drive_buf.as_slice()[..len] {
                return Err(anyhow!(io::Error::other("Verification failed")));
            }

            pos += len as u64;
            progress_mutex.lock().unwrap().done += len as u64;
        }
    }

    let mut progress = progress_mutex.lock().unwrap();
    progress.verify_secs = timer.elapsed().as_secs_f64();
    progress.finished = true;

    Ok(())
}

pub fn verify(
    image: &Volume,
    drive: &Volume,
    progress_mutex: &ProgressMutex,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    if let Some(bmap) = &image.bmap {
        return verify_mapped(image, drive, bmap, progress_mutex, cancelled);
    }

    let mut image_file = open_for_reading(image)?;
    let mut drive_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(&drive.path)?;

    let mut image_buffer = vec![0u8; BLOCK_SIZE];
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
    progress.verify_secs = timer.elapsed().as_secs_f64();
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
