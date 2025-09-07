//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::alloc::{alloc, Layout};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
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
	pub secs: u64,
	pub finished: bool,
}

impl Progress {
	pub fn percents(&self) -> f64 {
		if self.size == 0 {
			0.0
		} else {
			self.done as f64 / self.size as f64
		}
	}
}

pub type ProgressMutex = Arc<Mutex<Progress>>;

pub fn list_drives(all_drives: bool) -> Vec<Drive> {
	let mut drives = Vec::new();

	for device in drives::get_devices().unwrap() {
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
	drives
}

fn open_for_reading(vol: &Volume) -> io::Result<Box<dyn Read>> {
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

fn open_for_writing(vol: &Volume) -> io::Result<Box<dyn Write>> {
	let mut options = OpenOptions::new();
	let mut options = options.create(true).write(true).truncate(true);
	if vol.vtype == VolumeType::Drive {
		options = options.custom_flags(libc::O_DSYNC)
	}
	let file = options.open(&vol.path)?;

	let file: Box<dyn Write> = match vol.compression {
		Compression::None => Box::new(file),
		Compression::Gzip => Box::new(
			flate2::write::GzEncoder::new(file,
			flate2::Compression::default())),
		Compression::Bzip2 => Box::new(
			bzip2::write::BzEncoder::new(file,
			bzip2::Compression::default())),
		Compression::Xz => Box::new(
			xz2::write::XzEncoder::new(file, 3)),
		Compression::Zstd => Box::new(
			zstd::stream::write::Encoder::new(file,
			zstd::DEFAULT_COMPRESSION_LEVEL)?.auto_finish()),
	};

	Ok(file)
}

pub fn copy(src: &Volume, dest: &Volume, progress_mutex: &ProgressMutex) -> io::Result<()> {
	if src.vtype == VolumeType::Image
		&& src.size.is_some() && dest.size.is_some()
		&& src.size > dest.size {

		return Err(io::Error::other("File too large (os error 27)"));
	}

	let mut srcfile = open_for_reading(src)?;
	let mut destfile = open_for_writing(dest)?;
	let mut buffer = [0u8; BLOCK_SIZE];
	let timer = Instant::now();

	loop {
		let len = srcfile.read(&mut buffer)?;
		if len == 0 {
			break;
		}

		destfile.write_all(&buffer[..len])?;

		let mut progress = progress_mutex.lock().unwrap();
		progress.done += len as u64;

		if progress.size > 0 && progress.size == progress.done {
			break;
		}
	}

	let mut progress = progress_mutex.lock().unwrap();
	progress.secs = timer.elapsed().as_secs();
	progress.finished = true;

	Ok(())
}

pub fn verify(image: &Volume, drive: &Volume, progress_mutex: &ProgressMutex) -> io::Result<()> {
	let mut image_file = open_for_reading(image)?;
	let mut drive_file = OpenOptions::new()
		.read(true).custom_flags(libc::O_DIRECT).open(&drive.path)?;

	let mut image_buffer = [0u8; BLOCK_SIZE];
	let drive_buffer_ptr = unsafe {
		alloc(Layout::from_size_align(BLOCK_SIZE, 4096).unwrap())
	};
	let drive_buffer = unsafe {
		std::slice::from_raw_parts_mut(drive_buffer_ptr, BLOCK_SIZE)
	};

	let timer = Instant::now();

	loop {
		let len = match image_file.read_exact(&mut image_buffer) {
			Ok(_) => BLOCK_SIZE,
			Err(_) => image_file.read(&mut image_buffer)?,
		};

		if len == 0 {
			break;
		}

		let _ = drive_file.read(drive_buffer)?;

		if image_buffer[..len] != drive_buffer[..len] {
			return Err(io::Error::other("Verification failed"));
		}

		let mut progress = progress_mutex.lock().unwrap();
		progress.done += len as u64;
	}

	let mut progress = progress_mutex.lock().unwrap();
	progress.secs += timer.elapsed().as_secs();
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
