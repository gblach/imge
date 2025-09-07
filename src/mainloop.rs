//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use crate::imge;
use crate::Args;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use derivative::Derivative;
use num_format::{SystemLocale, ToFormattedString};
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::ffi::OsString;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::{fs, io};

#[derive(Default, PartialEq)]
enum Modal {
    #[default]
    None,
    Keybindings,
    Warning,
    Copying,
    Verifying,
    Victory,
    Error,
}

#[derive(Derivative)]
#[derivative(Default)]
pub struct Mainloop {
    args: Args,
    ui_accent: Style,
    image_basename: String,
    image_compression: imge::Compression,
    drives: Vec<imge::Drive>,
    selected_row: usize,
    selected_drive: Option<OsString>,
    selected_size: u64,
    modal: Modal,
    progress: Option<imge::ProgressMutex>,
    error: Arc<Mutex<Option<io::Error>>>,
    exit: bool,
}

impl Mainloop {
    pub fn new(args: Args) -> Self {
        let ui_accent = match args.from_drive {
            false => Style::new().magenta(),
            true => Style::new().yellow(),
        };

        let image_path = Path::new(&args.image);
        let image_basename = image_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let ext = image_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let image_compression = match ext.as_str() {
            "gz" => imge::Compression::Gzip,
            "bz2" => imge::Compression::Bzip2,
            "xz" => imge::Compression::Xz,
            "zst" => imge::Compression::Zstd,
            _ => imge::Compression::None,
        };

        Self {
            args: args.clone(),
            ui_accent,
            image_basename,
            image_compression,
            selected_drive: args.drive,
            ..Default::default()
        }
    }

    pub fn run(&mut self) -> io::Result<()> {
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

        self.update_drives(true);

        if self.args.drive.is_some() {
            self.start_copying();
        }

        while !self.exit {
            if self.error.lock().unwrap().is_some() {
                self.modal = Modal::Error;
            } else if let Some(progress) = &self.progress
                && progress.lock().unwrap().finished
            {
                if self.args.verify && self.modal == Modal::Copying {
                    self.start_verifying();
                } else if self.args.drive.is_none() {
                    self.modal = Modal::Victory;
                } else {
                    self.exit = true;
                }
            }

            terminal.draw(|frame| {
                self.render_window(frame);
                self.render_drives(frame);
                match self.modal {
                    Modal::Keybindings => self.render_keybindings(frame),
                    Modal::Warning => self.render_warning(frame),
                    Modal::Copying => self.render_copying(frame),
                    Modal::Verifying => self.render_verifying(frame),
                    Modal::Victory => self.render_victory(frame),
                    Modal::Error => self.render_error(frame),
                    _ => {}
                }
            })?;

            if event::poll(std::time::Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_events(key);
            }
        }

        Ok(())
    }

    fn render_window(&self, frame: &mut Frame) {
        let header = match self.args.from_drive {
            false => Line::from(vec![
                "Select the drive you wanna copy ".into(),
                Span::styled(&self.image_basename, self.ui_accent),
                " to.".into(),
            ]),
            true => Line::from(vec![
                "Select the drive you wanna copy to ".into(),
                Span::styled(&self.image_basename, self.ui_accent),
                ".".into(),
            ]),
        };

        let p = Paragraph::new(vec![header])
            .wrap(Wrap { trim: true })
            .centered();
        frame.render_widget(p, frame.area());

        if self.modal == Modal::None || self.modal == Modal::Keybindings {
            let info = Line::from(vec![
                "Press ".into(),
                Span::styled("<i>", self.ui_accent),
                " to display keybindings.".into(),
            ]);
            let area = Rect::new(0, frame.area().height - 1, frame.area().width, 1);
            frame.render_widget(info, area);
        }
    }

    fn render_drives(&self, frame: &mut Frame) {
        let mut rows = Vec::with_capacity(self.drives.len());

        for drive in &self.drives {
            let mut row = Vec::with_capacity(6);

            row.push(Cell::from(drive.name.to_string_lossy()));
            row.push(Cell::from(drive.model.clone()));
            if frame.area().width > 160 {
                row.push(Cell::from(drive.serial.clone()));
            }
            if frame.area().width > 80 {
                let is_removable = if drive.is_removable {
                    "Removable"
                } else {
                    "Non-removable"
                };
                row.push(Cell::from(is_removable));
            }
            if frame.area().width > 120 {
                let is_mounted = if drive.is_mounted {
                    "Mounted"
                } else {
                    "Unmounted"
                };
                row.push(Cell::from(is_mounted));
            }
            let size = imge::humanize(drive.size);
            row.push(Cell::from(Text::from(size).right_aligned()));

            rows.push(Row::new(row));
        }

        let mut widths: Vec<Constraint> = Vec::with_capacity(self.drives.len());

        widths.push(Constraint::Fill(2));
        widths.push(Constraint::Fill(3));
        if frame.area().width > 160 {
            widths.push(Constraint::Fill(2));
        }
        if frame.area().width > 80 {
            widths.push(Constraint::Fill(2));
        }
        if frame.area().width > 120 {
            widths.push(Constraint::Fill(2));
        }
        widths.push(Constraint::Fill(1));

        let table = Table::new(rows, widths)
            .highlight_symbol("-> ")
            .row_highlight_style(self.ui_accent);

        let mut state = TableState::default();
        state.select(Some(self.selected_row));

        let area = Rect::new(0, 3, frame.area().width, frame.area().height - 3);
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn render_modal(&self, frame: &mut Frame, title: &str, lines: Vec<Line>) {
        let block = Block::default()
            .title_top(title)
            .title_style(Style::new().add_modifier(Modifier::BOLD))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::new().dark_gray())
            .border_type(BorderType::Rounded);

        let p = Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .centered()
            .block(block);

        let w = 72;
        let h = 10;
        let x = (frame.area().width - w) / 2;
        let y = (frame.area().height - h) / 2;
        let area = Rect::new(x, y, w, h);

        frame.render_widget(p, area);
    }

    fn render_keybindings(&self, frame: &mut Frame) {
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("<a>      ", self.ui_accent),
                "Show all/removable drives        ".into(),
            ]),
            Line::from(vec![
                Span::styled("<r>      ", self.ui_accent),
                "Refresh drives                   ".into(),
            ]),
            Line::from(vec![
                Span::styled("<up>     ", self.ui_accent),
                "Select the drive above           ".into(),
            ]),
            Line::from(vec![
                Span::styled("<down>   ", self.ui_accent),
                "Select the drive below           ".into(),
            ]),
            Line::from(vec![
                Span::styled("<enter>  ", self.ui_accent),
                "Write the image to selected drive".into(),
            ]),
            Line::from(vec![
                Span::styled("<esc>    ", self.ui_accent),
                "Quit                             ".into(),
            ]),
        ];

        self.render_modal(frame, " Keybindings ", lines);
    }

    fn render_warning(&self, frame: &mut Frame) {
        let drive_path = self
            .selected_drive
            .clone()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let (src, dest) = match self.args.from_drive {
            false => (&self.image_basename, &drive_path),
            true => (&drive_path, &self.image_basename),
        };

        let mut lines = Vec::with_capacity(6);
        lines.push(Line::from(""));

        if self.args.from_drive {
            lines.push(Line::from(""));
        }

        lines.push(Line::from(vec![
            "Are you really going to copy ".into(),
            Span::styled(src, self.ui_accent),
            " to\u{00a0}".into(),
            Span::styled(dest, self.ui_accent),
            "?".into(),
        ]));

        if !self.args.from_drive {
            lines.push(Line::from("This is something that cannot be undone."));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(""));

        lines.push(Line::from(vec![
            Span::styled("<esc> ", self.ui_accent),
            "Cancel".into(),
            "          ".into(),
            Span::styled("<enter> ", self.ui_accent),
            "Continue".into(),
        ]));

        self.render_modal(frame, " Warning ", lines);
    }

    fn render_copying(&self, frame: &mut Frame) {
        let progress = self.progress.as_ref().unwrap().lock().unwrap();
        let area = Rect::new(1, (frame.area().height - 5) / 2, frame.area().width - 2, 5);

        if progress.size > 0 {
            let block = Block::default()
                .title_top(" Copying ")
                .title_style(Style::new().add_modifier(Modifier::BOLD))
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_style(Style::new().dark_gray())
                .border_type(BorderType::Rounded);

            let gauge = Gauge::default()
                .gauge_style(self.ui_accent)
                .style(Style::new().bold())
                .ratio(progress.percents())
                .label(format!("{:.1} %", progress.percents() * 100.0))
                .block(block);

            frame.render_widget(gauge, area);
        } else {
            let locale = SystemLocale::default().unwrap();
            let copied_bytes = format!(
                " {} bytes copied ",
                progress.done.to_formatted_string(&locale)
            );

            let lines = vec![
                Line::from(""),
                Line::from(""),
                Line::from(""),
                Line::from(Span::styled(copied_bytes, self.ui_accent)),
            ];

            self.render_modal(frame, " Copying ", lines);
        }
    }

    fn render_verifying(&self, frame: &mut Frame) {
        let progress = self.progress.as_ref().unwrap().lock().unwrap();
        let area = Rect::new(1, (frame.area().height - 5) / 2, frame.area().width - 2, 5);

        let block = Block::default()
            .title_top(" Verifying ")
            .title_style(Style::new().add_modifier(Modifier::BOLD))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::new().dark_gray())
            .border_type(BorderType::Rounded);

        let gauge = Gauge::default()
            .gauge_style(Style::new().blue())
            .style(Style::new().bold())
            .ratio(progress.percents())
            .label(format!("{:.1} %", progress.percents() * 100.0))
            .block(block);

        frame.render_widget(gauge, area);
    }

    fn render_victory(&self, frame: &mut Frame) {
        let progress = self.progress.as_ref().unwrap().lock().unwrap();

        let speed = if progress.secs > 0 {
            progress.done / progress.secs
        } else {
            progress.done
        };

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                if !self.args.verify {
                    "Copied ".into()
                } else {
                    "Copied and verified ".into()
                },
                Span::styled(imge::humanize(progress.done), self.ui_accent),
                " in ".into(),
                Span::styled(progress.secs.to_string(), self.ui_accent),
                " seconds.".into(),
            ]),
            Line::from(""),
            Line::from(vec![
                "An average of ".into(),
                Span::styled(imge::humanize(speed), self.ui_accent),
                " per second.".into(),
            ]),
            Line::from(""),
            Line::from(""),
            Line::from(vec![Span::styled("<esc> ", self.ui_accent), "Close".into()]),
        ];

        self.render_modal(frame, " Victory ", lines);
    }

    fn render_error(&self, frame: &mut Frame) {
        let error = self.error.lock().unwrap();

        let lines = vec![
            Line::from(""),
            Line::from(Span::raw(error.as_ref().unwrap().to_string())),
            Line::from(""),
            Line::from(""),
            Line::from(vec![Span::styled("<esc> ", self.ui_accent), "Close".into()]),
        ];

        self.render_modal(frame, " Error ", lines);
    }

    fn handle_events(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.exit = true;
        } else if key.code == KeyCode::Char('i') {
            match self.modal {
                Modal::None => self.modal = Modal::Keybindings,
                Modal::Keybindings => self.modal = Modal::None,
                _ => {}
            }
        } else if self.modal == Modal::Warning && key.code == KeyCode::Enter {
            self.start_copying();
        } else if self.modal == Modal::None {
            match key.code {
                KeyCode::Char('a') => {
                    self.args.all_drives = !self.args.all_drives;
                    self.update_drives(true);
                }
                KeyCode::Char('r') => {
                    self.update_drives(true);
                }
                KeyCode::Up => {
                    if self.selected_row > 0 {
                        self.selected_row -= 1;
                        self.update_drives(false);
                    }
                }
                KeyCode::Down => {
                    if self.selected_row + 1 < self.drives.len() {
                        self.selected_row += 1;
                        self.update_drives(false);
                    }
                }
                KeyCode::Enter => {
                    if self.selected_drive.is_some() {
                        self.modal = Modal::Warning;
                    }
                }
                KeyCode::Esc => {
                    self.exit = true;
                }
                _ => {}
            }
        } else if key.code == KeyCode::Esc {
            self.modal = Modal::None;
            self.progress = None;
            *self.error.lock().unwrap() = None;
        }
    }

    fn update_drives(&mut self, refresh: bool) {
        if refresh {
            self.drives = imge::list_drives(self.args.all_drives);
            self.selected_row = 0;

            for i in 0..self.drives.len() {
                if self.selected_drive == Some(self.drives[i].name.clone()) {
                    self.selected_row = i;
                    break;
                }
            }

            if !self.drives.is_empty() && self.selected_row > self.drives.len() - 1 {
                self.selected_row = self.drives.len() - 1;
            }

            if self.drives.is_empty() {
                self.selected_drive = None;
                self.selected_size = 0;
                return;
            }
        }

        self.selected_drive
            .clone_from(&Some(self.drives[self.selected_row].name.clone()));
        self.selected_size = self.drives[self.selected_row].size;
    }

    fn get_volumes(&self) -> (imge::Volume, imge::Volume) {
        let image = imge::Volume {
            vtype: imge::VolumeType::Image,
            path: self.args.image.clone(),
            size: if self.image_compression == imge::Compression::None {
                match fs::metadata(&self.args.image) {
                    Ok(metadata) => {
                        if metadata.file_type().is_char_device() {
                            Some(self.selected_size)
                        } else {
                            Some(metadata.len())
                        }
                    }
                    Err(_) => None,
                }
            } else {
                None
            },
            compression: self.image_compression,
        };

        let drive = imge::Volume {
            vtype: imge::VolumeType::Drive,
            path: self.selected_drive.clone().unwrap(),
            size: Some(self.selected_size),
            compression: imge::Compression::None,
        };

        (image, drive)
    }

    fn start_copying(&mut self) {
        let (image, drive) = self.get_volumes();
        let error = self.error.clone();

        let (src, dest) = match self.args.from_drive {
            false => (image, drive),
            true => (drive, image),
        };

        let progress = Arc::new(Mutex::new(imge::Progress {
            size: src.size.unwrap_or_default(),
            ..Default::default()
        }));

        self.progress = Some(progress.clone());
        self.modal = Modal::Copying;

        thread::spawn(move || {
            let result = imge::copy(&src, &dest, &progress);
            if let Err(err) = result {
                *error.lock().unwrap() = Some(err);
            }
        });
    }

    fn start_verifying(&mut self) {
        let (image, drive) = self.get_volumes();
        let error = self.error.clone();

        if fs::metadata(&image.path)
            .unwrap()
            .file_type()
            .is_char_device()
        {
            self.modal = Modal::Victory;
            return;
        }

        let copying_progress = self.progress.as_ref().unwrap().lock().unwrap();
        let progress = Arc::new(Mutex::new(imge::Progress {
            size: if copying_progress.size > 0 {
                copying_progress.size
            } else {
                copying_progress.done
            },
            secs: copying_progress.secs,
            ..Default::default()
        }));
        drop(copying_progress);

        self.progress = Some(progress.clone());
        self.modal = Modal::Verifying;

        thread::spawn(move || {
            let result = imge::verify(&image, &drive, &progress);
            if let Err(err) = result {
                *error.lock().unwrap() = Some(err);
            }
        });
    }
}
