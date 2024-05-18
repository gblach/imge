//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

mod imge;
mod mainloop;

use argh::FromArgs;
use crossterm::terminal;
use mainloop::Mainloop;
use std::fs::File;
use std::io;

#[derive(FromArgs)]
/// Write disk images to physical drive.
struct Args {
	/// show all drives
	#[argh(switch, short='a')]
	all_drives: bool,

	/// magenta mode
	#[argh(switch, short='m')]
	magenta: bool,

	/// path to image
	#[argh(positional)]
	image: String,
}

fn terminal_raw_mode(raw_mode: bool) -> io::Result<()> {
	if raw_mode {
		crossterm::execute!(io::stdout(), terminal::EnterAlternateScreen)?;
		terminal::enable_raw_mode()?;
	} else {
		crossterm::execute!(io::stdout(), terminal::LeaveAlternateScreen)?;
		terminal::disable_raw_mode()?;
	}

	Ok(())
}

fn main() -> io::Result<()> {
	let args: Args = argh::from_env();
	File::open(&args.image)?;

	terminal_raw_mode(true)?;
	Mainloop::new(args).run()?;
	terminal_raw_mode(false)?;

	Ok(())
}
