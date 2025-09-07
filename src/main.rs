//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

mod imge;
mod mainloop;

use argp::FromArgs;
use crossterm::terminal;
use mainloop::Mainloop;
use std::ffi::OsString;
use std::fs::{remove_file, File};
use std::io;
use std::path::Path;

#[derive(Clone, Default, FromArgs)]
/// Write disk images to physical drive or vice versa.
struct Args {
    /// show all drives
    #[argp(switch, short = 'a')]
    all_drives: bool,

    /// use this drive, do not ask
    #[argp(option, short = 'd')]
    drive: Option<OsString>,

    /// copy drive to image (instead of image to drive)
    #[argp(switch, short = 'f')]
    from_drive: bool,

    /// verify if data was copied correctly
    #[argp(switch, short = 'v')]
    verify: bool,

    /// path to image
    #[argp(positional)]
    image: OsString,
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
    let args: Args = argp::parse_args_or_exit(argp::DEFAULT);

    if args.from_drive {
        let path = Path::new(&args.image);
        let dirname = path.parent().unwrap().to_string_lossy();
        let filename = path.file_name().unwrap().to_string_lossy();
        let write_test = if dirname.is_empty() {
            format!(".{filename}.imge")
        } else {
            format!("{dirname}/.{filename}.imge")
        };
        File::create(&write_test)?;
        remove_file(&write_test)?;
    } else {
        File::open(&args.image)?;
    }

    terminal_raw_mode(true)?;
    Mainloop::new(args).run()?;
    terminal_raw_mode(false)?;

    Ok(())
}
