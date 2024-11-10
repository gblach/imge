//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use libarchive3_sys::ffi::*;
use mime::Mime;
use std::alloc::{alloc, Layout};
use std::ffi::{c_void, CString, OsString};
use std::fs::{File, OpenOptions};
use std::io::prelude::*;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
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

pub fn list(all_drives: bool) -> Vec<Drive> {
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

pub struct Path {
	pub path: OsString,
	pub size: Option<u64>,
}

fn libarchive_open_for_reading(path: &Path)
	-> io::Result<(*mut Struct_archive, *mut Struct_archive_entry)> {

	let filename = CString::new(path.path.as_bytes()).unwrap();

	unsafe {
		let a = archive_read_new();
		archive_read_support_filter_all(a);
		archive_read_support_format_raw(a);

		let rc = archive_read_open_filename(a, filename.as_ptr(), BLOCK_SIZE);
		if rc != ARCHIVE_OK {
			return Err(io::Error::other("Cannot open file for reading"));
		}

		let ae = archive_entry_new2(a);
		let ae_ptr = ae as *mut *mut Struct_archive_entry;
		let rc = archive_read_next_header(a, ae_ptr);
		if rc != ARCHIVE_OK {
			return Err(io::Error::other("Cannot read file header"));
		}

		Ok((a, ae))
	}
}

fn libarchive_open_for_writing(path: &Path, image_mime_type: Mime)
	-> io::Result<(*mut Struct_archive, *mut Struct_archive_entry)> {

	let filename = CString::new(path.path.as_bytes()).unwrap();

	unsafe {
		let a = archive_write_new();
		let filter = match image_mime_type.essence_str() {
			"application/gzip" => 1,
			"application/x-bzip2" => 2,
			"application/x-xz" => 6,
			"application/zstd" => 14,
			_ => 0,
		};
		archive_write_add_filter(a, filter);
		archive_write_set_format(a, 0x90000); // RAW

		let rc = archive_write_open_filename(a, filename.as_ptr());
		if rc != ARCHIVE_OK {
			return Err(io::Error::other("Cannot open file for writing"));
		}

		let ae = archive_entry_new2(a);
		archive_entry_set_filetype(ae, AE_IFREG);
		let rc = archive_write_header(a, ae);
		if rc != ARCHIVE_OK {
			return Err(io::Error::other("Cannot write file header"));
		}

		Ok((a, ae))
	}
}

fn libarchive_close_for_reading(a: *mut Struct_archive) {
	unsafe {
		archive_read_close(a);
		archive_read_free(a);
	}
}

fn libarchive_close_for_writing(a: *mut Struct_archive, ae: *mut Struct_archive_entry) {
	unsafe {
		archive_entry_free(ae);
		archive_write_close(a);
		archive_write_free(a);
	}
}

pub fn copy(src: &Path, dest: &Path, from_drive: bool, image_mime_type: Mime,
	progress_mutex: &Arc<Mutex<Progress>>) -> io::Result<()> {

	if let (Some(ssize), Some(dsize)) = (src.size, dest.size) {
		if !from_drive && ssize > dsize {
			return Err(io::Error::other("File too large (os error 27)"));
		}
	}

	let mut srcfile = File::open(&src.path)?;
	let mut destfile = OpenOptions::new()
		.create(true).write(true).truncate(true)
		.custom_flags(libc::O_DSYNC).open(&dest.path)?;

	let (a, ae) = match from_drive {
		false => libarchive_open_for_reading(src)?,
		true => libarchive_open_for_writing(dest, image_mime_type.clone())?,
	};

	let mut buffer = [0; BLOCK_SIZE];
	let buffer_ptr = buffer.as_mut_ptr() as *mut c_void;

	let timer = Instant::now();

	loop {
		let len = match from_drive {
			false => unsafe {
				archive_read_data(a, buffer_ptr, BLOCK_SIZE)
			},
			true => srcfile.read(&mut buffer)? as isize,
		};
		if len <= 0 {
			break;
		}

		if from_drive && image_mime_type != mime::APPLICATION_OCTET_STREAM {
			unsafe {
				archive_write_data(a, buffer_ptr, len as usize);
			}
		} else {
			destfile.write_all(&buffer[..(len as usize)])?;
		}

		let mut progress = progress_mutex.lock().unwrap();
		progress.done += len as u64;
	}

	match from_drive {
		false => libarchive_close_for_reading(a),
		true => libarchive_close_for_writing(a, ae),
	};

	let mut progress = progress_mutex.lock().unwrap();
	progress.secs = timer.elapsed().as_secs();
	progress.finished = true;

	Ok(())
}

pub fn verify(src: &Path, dest: &Path, from_drive: bool,
	progress_mutex: &Arc<Mutex<Progress>>) -> io::Result<()> {

	let mut srcfile = File::open(&src.path)?;
	let mut destfile = OpenOptions::new().read(true)
		.custom_flags(libc::O_DIRECT).open(&dest.path)?;

	let (a, _ae) = match from_drive {
		false => libarchive_open_for_reading(src)?,
		true => libarchive_open_for_reading(dest)?,
	};


	let mut srcbuffer = [0; BLOCK_SIZE];
	let srcbuffer_ptr = srcbuffer.as_mut_ptr() as *mut c_void;
	let destbuffer_ptr = unsafe {
		alloc(Layout::from_size_align(BLOCK_SIZE, BLOCK_SIZE).unwrap())
	};
	let destbuffer = unsafe {
		std::slice::from_raw_parts_mut(destbuffer_ptr, BLOCK_SIZE)
	};

	let timer = Instant::now();

	loop {
		let len = match from_drive {
			false => unsafe {
				archive_read_data(a, srcbuffer_ptr, BLOCK_SIZE)
			},
			true => srcfile.read(&mut srcbuffer)? as isize,
		};
		if len <= 0 {
			break;
		}

		match from_drive {
			false => destfile.read(destbuffer)? as isize,
			true => unsafe {
				archive_read_data(a, destbuffer_ptr as *mut c_void, BLOCK_SIZE)
			},
		};

		if srcbuffer[..len as usize] != destbuffer[..len as usize] {
			return Err(io::Error::other("Verification failed"));
		}

		let mut progress = progress_mutex.lock().unwrap();
		progress.done += len as u64;
	}

	libarchive_close_for_reading(a);

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
