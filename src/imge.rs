//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use sha2::digest::DynDigest;
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

const BLOCK_SIZE: usize = 4 * 1024 * 1024;
// Lets the reader run a few blocks ahead of the drive without using much memory.
const PIPELINE_BUFFERS: usize = 4;
const O_DIRECT: i32 = OFlags::DIRECT.bits() as i32;

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
        Compression::Xz => Box::new(liblzma::read::XzDecoder::new(file)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(file)?),
    };

    Ok(file)
}

struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuf {
    fn new(size: usize, align: usize) -> Result<Self> {
        let layout = Layout::from_size_align(size, align)?;
        let ptr = unsafe { alloc_zeroed(layout) };
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

// The buffer owns its allocation exclusively, like a Box<[u8]> does.
unsafe impl Send for AlignedBuf {}

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
    liblzma::write::XzEncoder<File>,
    zstd::stream::write::Encoder<'static, File>
);

// O_DIRECT rejects writes whose buffer, length or offset are not block aligned. Only the tail
// of an image can be like that, so O_DIRECT is turned off for it instead of padding the write.
fn disable_direct_if_unaligned(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    if (buf.as_ptr() as usize | buf.len()).is_multiple_of(4096) && offset.is_multiple_of(4096) {
        return Ok(());
    }

    let flags = fcntl_getfl(file)?;
    fcntl_setfl(file, flags - OFlags::DIRECT)?;
    Ok(())
}

struct DirectFile {
    file: File,
    pos: u64,
}

impl Write for DirectFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        disable_direct_if_unaligned(&self.file, buf, self.pos)?;
        let len = self.file.write(buf)?;
        self.pos += len as u64;
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

// O_DIRECT skips the page cache, but the data can still sit in the drive's own write cache.
impl FinishWrite for DirectFile {
    fn finish(self: Box<Self>) -> io::Result<()> {
        self.file.sync_all()
    }
}

fn open_for_writing(vol: &Volume) -> Result<Box<dyn FinishWrite>> {
    let mut options = OpenOptions::new();
    let mut options = options.create(true).write(true).truncate(true);
    if vol.vtype == VolumeType::Drive {
        options = options.custom_flags(O_DIRECT)
    }
    let file = options.open(&vol.path)?;

    let file: Box<dyn FinishWrite> = match vol.compression {
        Compression::None if vol.vtype == VolumeType::Drive => {
            Box::new(DirectFile { file, pos: 0 })
        }
        Compression::None => Box::new(file),
        Compression::Gzip => Box::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::default(),
        )),
        Compression::Bzip2 => Box::new(bzip2::write::BzEncoder::new(
            file,
            bzip2::Compression::default(),
        )),
        Compression::Xz => Box::new(liblzma::write::XzEncoder::new(file, 3)),
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

struct Chunk {
    buf: AlignedBuf,
    len: usize,
    pos: u64,
}

struct Producer {
    free: Receiver<AlignedBuf>,
    full: Sender<Chunk>,
}

impl Producer {
    // None and false mean the consumer has stopped, so the producer should stop too.
    fn buffer(&self) -> Option<AlignedBuf> {
        self.free.recv().ok()
    }

    fn send(&self, buf: AlignedBuf, len: usize, pos: u64) -> bool {
        self.full.send(Chunk { buf, len, pos }).is_ok()
    }
}

// Runs produce on its own thread, so reading and decompressing the source overlaps with the
// drive I/O done by consume. Buffers are passed back and forth, which bounds memory use.
fn pipeline(
    cancelled: &Arc<AtomicBool>,
    produce: impl FnOnce(&Producer) -> Result<()> + Send,
    mut consume: impl FnMut(&[u8], u64) -> Result<()>,
) -> Result<()> {
    let (free_tx, free_rx) = mpsc::channel();
    let (full_tx, full_rx) = mpsc::channel();
    for _ in 0..PIPELINE_BUFFERS {
        free_tx.send(AlignedBuf::new(BLOCK_SIZE, 4096)?).unwrap();
    }

    thread::scope(|s| {
        let reader = s.spawn(move || {
            produce(&Producer {
                free: free_rx,
                full: full_tx,
            })
        });

        let mut result = Ok(());
        for chunk in full_rx {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            result = consume(&chunk.buf.as_slice()[..chunk.len], chunk.pos);
            if result.is_err() {
                break;
            }
            let _ = free_tx.send(chunk.buf);
        }

        // Unblocks a reader waiting for a free buffer after the loop stopped early.
        drop(free_tx);
        let produced = reader.join().unwrap();
        result.and(produced)
    })
}

fn produce_stream(src: &mut dyn Read, size: u64, p: &Producer) -> Result<()> {
    let mut pos = 0;

    // A char-device source never reaches EOF, so stop exactly at the known size.
    while size == 0 || pos < size {
        let Some(mut buf) = p.buffer() else {
            break;
        };
        let want = if size > 0 {
            (size - pos).min(BLOCK_SIZE as u64) as usize
        } else {
            BLOCK_SIZE
        };

        // Short reads from a decompressor would leave unaligned writes, so fill whole blocks.
        let len = read_full(src, &mut buf.as_mut_slice()[..want])?;
        if len == 0 || !p.send(buf, len, pos) {
            break;
        }
        pos += len as u64;
    }

    Ok(())
}

fn produce_mapped(
    src: &mut dyn Read,
    bmap: &Bmap,
    p: &Producer,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let mut skipped = vec![0u8; BLOCK_SIZE];
    let mut hasher = bmap.checksum_type.hasher();
    let mut pos = 0;

    for range in &bmap.ranges {
        let (start, end) = bmap.byte_range(range);
        discard(src, start - pos, &mut skipped, cancelled)?;
        pos = start;

        while pos < end {
            let Some(mut buf) = p.buffer() else {
                return Ok(());
            };
            let len = (end - pos).min(BLOCK_SIZE as u64) as usize;
            let data = &mut buf.as_mut_slice()[..len];
            src.read_exact(data)?;
            hasher.update(data);
            if !p.send(buf, len, pos) {
                return Ok(());
            }
            pos += len as u64;
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

    Ok(())
}

fn copy_mapped(
    src: &Volume,
    dest: &Volume,
    bmap: &Bmap,
    progress_mutex: &ProgressMutex,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let srcfile = File::open(&src.path)?;
    let destfile = OpenOptions::new()
        .write(true)
        .custom_flags(O_DIRECT)
        .open(&dest.path)?;
    let mut done = 0;
    let timer = Instant::now();

    pipeline(
        cancelled,
        |p| {
            produce_mapped(
                &mut decompress(srcfile, src.compression)?,
                bmap,
                p,
                cancelled,
            )
        },
        |data, pos| {
            disable_direct_if_unaligned(&destfile, data, pos)?;
            destfile.write_all_at(data, pos)?;
            done += data.len() as u64;
            progress_mutex.lock().unwrap().done = done;
            Ok(())
        },
    )?;

    if cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }

    destfile.sync_all()?;

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
        return Err(anyhow!(io::Error::from(Errno::FBIG)));
    }

    if let Some(bmap) = &src.bmap {
        return copy_mapped(src, dest, bmap, progress_mutex, cancelled);
    }

    let compressed_done = Arc::new(AtomicU64::new(0));
    let counting_file = CountingReader {
        inner: File::open(&src.path)?,
        count: compressed_done.clone(),
    };
    let mut destfile = open_for_writing(dest)?;
    let size = src.size.unwrap_or_default();
    let mut done = 0;
    let timer = Instant::now();

    pipeline(
        cancelled,
        |p| produce_stream(&mut decompress(counting_file, src.compression)?, size, p),
        |data, _| {
            destfile.write_all(data)?;
            done += data.len() as u64;
            let mut progress = progress_mutex.lock().unwrap();
            progress.done = done;
            progress.compressed_done = compressed_done.load(Ordering::Relaxed);
            Ok(())
        },
    )?;

    if cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }

    destfile.finish()?;

    let mut progress = progress_mutex.lock().unwrap();
    progress.copy_secs = timer.elapsed().as_secs_f64();
    progress.finished = true;

    Ok(())
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
    let image_file = File::open(&image.path)?;
    let drive_file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECT)
        .open(&drive.path)?;

    let mut drive_buf = AlignedBuf::new(BLOCK_SIZE, 4096)?;

    let timer = Instant::now();

    pipeline(
        cancelled,
        |p| {
            produce_mapped(
                &mut decompress(image_file, image.compression)?,
                bmap,
                p,
                cancelled,
            )
        },
        |data, pos| {
            let len = data.len();

            // O_DIRECT needs the read length aligned too, so read a bit past the range end.
            let drive_slice = &mut drive_buf.as_mut_slice()[..len.next_multiple_of(4096)];
            let drive_len = read_full_at(&drive_file, drive_slice, pos)?;

            if drive_len < len || data != &drive_buf.as_slice()[..len] {
                return Err(anyhow!(io::Error::other("Verification failed")));
            }

            progress_mutex.lock().unwrap().done += len as u64;
            Ok(())
        },
    )?;

    if cancelled.load(Ordering::Relaxed) {
        return Ok(());
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

    let image_file = File::open(&image.path)?;
    let mut drive_file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECT)
        .open(&drive.path)?;

    let mut drive_buf = AlignedBuf::new(BLOCK_SIZE, 4096)?;
    let size = image.size.unwrap_or_default();

    let timer = Instant::now();

    pipeline(
        cancelled,
        |p| produce_stream(&mut decompress(image_file, image.compression)?, size, p),
        |data, _| {
            let len = data.len();
            let drive_len = read_full(&mut drive_file, drive_buf.as_mut_slice())?;

            if drive_len < len || data != &drive_buf.as_slice()[..len] {
                return Err(anyhow!(io::Error::other("Verification failed")));
            }

            progress_mutex.lock().unwrap().done += len as u64;
            Ok(())
        },
    )?;

    if cancelled.load(Ordering::Relaxed) {
        return Ok(());
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
