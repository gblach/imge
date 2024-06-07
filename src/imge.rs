//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct Drive {
	pub name: String,
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
				name: format!("/dev/{}", device.name),
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
	pub copied: u64,
	pub size: u64,
	pub secs: u64,
}

impl Progress {
	pub fn percents(&self) -> f64 {
		if self.size == 0 {
			0.0
		} else {
			self.copied as f64 / self.size as f64
		}
	}

	pub fn finished(&self) -> bool {
		self.copied == self.size && self.size > 0
	}
}

pub struct Path {
	pub path: String,
	pub size: Option<u64>,
}

pub fn copy(src: &Path, dest: &Path, progress_mutex: &Arc<Mutex<Progress>>) -> io::Result<()> {
	let mut srcfile = File::open(&src.path)?;

	let ssize = match src.size {
		Some(size) => size,
		None => srcfile.metadata()?.len(),
	};

	if let Some(dsize) = dest.size {
		if ssize > dsize {
			return Err(io::Error::other("File too large (os error 27)"));
		}
	}

	let mut destfile = File::create(&dest.path)?;
	let mut buffer = [0; 1024 * 1024];
	let timer = Instant::now();

	let mut progress = progress_mutex.lock().unwrap();
	progress.size = ssize;
	drop(progress);

	loop {
		let len = srcfile.read(&mut buffer)?;
		if len == 0 {
			break;
		}
		destfile.write_all(&buffer[..len])?;
		destfile.sync_data()?;

		let mut progress = progress_mutex.lock().unwrap();
		progress.copied += len as u64;
	}

	let mut progress = progress_mutex.lock().unwrap();
	progress.secs = timer.elapsed().as_secs();

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
