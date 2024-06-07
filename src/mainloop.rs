//  This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{*, block::*};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use super::Args;
use super::imge;

#[derive(Default, PartialEq)]
enum Modal {
	#[default]
	None,
	Keybindings,
	Warning,
	Progress,
	Victory,
	Error,
}

#[derive(Default)]
pub struct Mainloop {
	args: Args,
	ui_accent: Style,
	image_basename: String,
	drives: Vec<imge::Drive>,
	selected_row: usize,
	selected_name: String,
	selected_size: u64,
	modal: Modal,
	progress: Option<Arc<Mutex<imge::Progress>>>,
	error: Arc<Mutex<Option<io::Error>>>,
	exit: bool,
}

impl Mainloop {
	pub fn new(args: Args) -> Self {
		let image_basename = Path::new(&args.image).file_name()
			.unwrap().to_string_lossy().to_string();

		Self {
			args: args.clone(),
			ui_accent: if args.from_drive {
				Style::new().light_yellow()
			} else {
				Style::new().light_magenta()
			},
			image_basename,
			..Default::default()
		}
	}

	pub fn run(&mut self) -> io::Result<()> {
		let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

		self.update_drives(true);

		while !self.exit {
			if self.error.lock().unwrap().is_some() {
				self.modal = Modal::Error;
			}

			else if let Some(progress) = &self.progress {
				if progress.lock().unwrap().finished() {
					self.modal = Modal::Victory;
				}
			}

			terminal.draw(|frame| {
				self.render_window(frame);
				self.render_drives(frame);
				if self.progress.is_some() {
					self.render_progress(frame);
				}
				match self.modal {
					Modal::Keybindings => self.render_keybindings(frame),
					Modal::Warning => self.render_warning(frame),
					Modal::Victory => self.render_victory(frame),
					Modal::Error => self.render_error(frame),
					_ => {},
				}
			})?;

			if event::poll(std::time::Duration::from_millis(100))? {
				if let Event::Key(key) = event::read()? {
					if key.kind == KeyEventKind::Press {
						self.handle_events(key);
					}
				}
			}
		}

		Ok(())
	}

	fn render_window(&self, frame: &mut Frame) {
		let block = Block::default()
			.borders(Borders::BOTTOM)
			.border_style(Style::new().dark_gray())
			.border_type(BorderType::Double);

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
			.wrap(Wrap { trim: true }).centered().block(block);
		frame.render_widget(p, frame.size());

		if self.modal == Modal::None || self.modal == Modal::Keybindings {
			let info = Line::from(vec![
				"Press ".into(),
				Span::styled("<i>", self.ui_accent),
				" to display keybindings.".into()
			]);
			let area = Rect::new(0, frame.size().height-2, frame.size().width, 1);
			frame.render_widget(info, area);
		}
	}

	fn render_drives(&self, frame: &mut Frame) {
		let mut rows = Vec::with_capacity(self.drives.len());

		for drive in &self.drives {
			let mut row = Vec::with_capacity(6);

			row.push(Cell::from(drive.name.clone()));
			row.push(Cell::from(drive.model.clone()));
			if frame.size().width > 160 {
				row.push(Cell::from(drive.serial.clone()));
			}
			if frame.size().width > 80 {
				let is_removable = if drive.is_removable {
					"Removable"
				} else {
					"Non-removable"
				};
				row.push(Cell::from(is_removable));
			}
			if frame.size().width > 120 {
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
		if frame.size().width > 160 {
			widths.push(Constraint::Fill(2));
		}
		if frame.size().width > 80 {
			widths.push(Constraint::Fill(2));
		}
		if frame.size().width > 120 {
			widths.push(Constraint::Fill(2));
		}
		widths.push(Constraint::Fill(1));

		let table = Table::new(rows, widths)
			.highlight_symbol("-> ")
			.highlight_style(self.ui_accent);

		let mut state = TableState::default();
		state.select(Some(self.selected_row));

		let area = Rect::new(0, 3, frame.size().width, frame.size().height-3);
		frame.render_stateful_widget(table, area, &mut state);
	}

	fn render_modal(&self, frame: &mut Frame, title: &str, lines: Vec<Line>) {
		let block = Block::default()
			.title(Title::from(title.bold()).alignment(Alignment::Center))
			.borders(Borders::ALL)
			.border_style(Style::new().dark_gray())
			.border_type(BorderType::Rounded);

		let p = Paragraph::new(lines).wrap(Wrap { trim: true }).centered().block(block);

		let w = 72;
		let h = 10;
		let x = (frame.size().width - w) / 2;
		let y = (frame.size().height - h) / 2;
		let area = Rect::new(x, y, w, h);

		frame.render_widget(p, area);
	}

	fn render_keybindings(&self, frame: &mut Frame) {
		let lines = vec![
			Line::from(vec![]),
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
		let (src, dest) = match self.args.from_drive {
			false => (&self.image_basename, &self.selected_name),
			true => (&self.selected_name, &self.image_basename),
		};

		let mut lines = Vec::with_capacity(6);
		lines.push(Line::from(vec![]));

		if self.args.from_drive {
			lines.push(Line::from(vec![]));
		}

		lines.push(Line::from(vec![
			"Are you really going to copy ".into(),
			Span::styled(src, self.ui_accent),
			" to\u{00a0}".into(),
			Span::styled(dest, self.ui_accent),
			"?".into()
		]));

		if !self.args.from_drive {
			lines.push(Line::from(vec![
				"This is something that cannot be undone.".into(),
			]));
		}

		lines.push(Line::from(vec![]));
		lines.push(Line::from(vec![]));

		lines.push(Line::from(vec![
			Span::styled("<esc> ", self.ui_accent),
			"Cancel".into(),
			"          ".into(),
			Span::styled("<enter> ", self.ui_accent),
			"Continue".into(),
		]));

		self.render_modal(frame, " Warning ", lines);
	}

	fn render_progress(&self, frame: &mut Frame) {
		let progress = self.progress.as_ref().unwrap().lock().unwrap();

		let gauge = LineGauge::default()
			.style(self.ui_accent)
			.gauge_style(self.ui_accent.on_dark_gray())
			.line_set(symbols::line::DOUBLE)
			.ratio(progress.percents());

		let area = Rect::new(0, frame.size().height-1, frame.size().width, 1);
		frame.render_widget(Text::from("     "), area);
    		frame.render_widget(gauge, area);
	}

	fn render_victory(&self, frame: &mut Frame) {
		let progress = self.progress.as_ref().unwrap().lock().unwrap();

		let speed = if progress.secs > 0 {
			progress.copied / progress.secs
		} else {
			progress.copied
		};

		let lines = vec![
			Line::from(vec![]),
			Line::from(vec![
				"Copied ".into(),
				Span::styled(imge::humanize(progress.copied), self.ui_accent),
				" in ".into(),
				Span::styled(progress.secs.to_string(), self.ui_accent),
				" seconds.".into(),
			]),
			Line::from(vec![]),
			Line::from(vec![
				"An average of ".into(),
				Span::styled(imge::humanize(speed), self.ui_accent),
				" per second.".into(),
			]),
			Line::from(vec![]),
			Line::from(vec![]),
			Line::from(vec![
				Span::styled("<esc> ", self.ui_accent),
				"Close".into(),
			]),
		];

		self.render_modal(frame, " Victory ", lines);
	}

	fn render_error(&self, frame: &mut Frame) {
		let error = self.error.lock().unwrap();

		let lines = vec![
			Line::from(vec![]),
			Line::from(vec![
				Span::raw(error.as_ref().unwrap().to_string()),
			]),
			Line::from(vec![]),
			Line::from(vec![]),
			Line::from(vec![
				Span::styled("<esc> ", self.ui_accent),
				"Close".into(),
			]),
		];

		self.render_modal(frame, " Error ", lines);
	}

	fn handle_events(&mut self, key: KeyEvent) {
		if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
			self.exit = true;
		}

		else if key.code == KeyCode::Char('i') {
			match self.modal {
				Modal::None => self.modal = Modal::Keybindings,
				Modal::Keybindings => self.modal = Modal::None,
				_ => {},
			}
		}

		else if self.modal == Modal::Warning && key.code == KeyCode::Enter {
			let image_path = imge::Path {
				path: self.args.image.clone(),
				size: None,
			};
			let drive_path = imge::Path {
				path: self.selected_name.clone(),
				size: Some(self.selected_size),
			};
			let (src, dest) = match self.args.from_drive {
				false => (image_path, drive_path),
				true => (drive_path, image_path),
			};

			let progress = Arc::new(Mutex::new(imge::Progress::default()));
			let error = self.error.clone();
			self.progress = Some(progress.clone());
			self.modal = Modal::Progress;

			thread::spawn(move || {
				if let Err(copy_err) = imge::copy(&src, &dest, &progress) {
					*error.lock().unwrap() = Some(copy_err);
				}
			});
		}

		else if self.modal == Modal::None {
			match key.code {
				KeyCode::Char('a') => {
					self.args.all_drives = !self.args.all_drives;
					self.update_drives(true);
				},
				KeyCode::Char('r') => {
					self.update_drives(true);
				},
				KeyCode::Up => {
					if self.selected_row > 0 {
						self.selected_row -= 1;
						self.update_drives(false);
					}
				},
				KeyCode::Down => {
					if self.selected_row + 1 < self.drives.len() {
						self.selected_row += 1;
						self.update_drives(false);
					}
				},
				KeyCode::Enter => {
					if !self.selected_name.is_empty() {
						self.modal = Modal::Warning;
					}
				},
				KeyCode::Esc => {
					self.exit = true;
				},
				_ => {}
			}

			println!("{:?}, {:?}, {:?}, {:?}",
				key.code, key.modifiers,
				key.kind, key.state);
		}

		else if key.code == KeyCode::Esc {
			self.modal = Modal::None;
			self.progress = None;
			*self.error.lock().unwrap() = None;
		}
	}

	fn update_drives(&mut self, refresh: bool) {
		if refresh {
			self.drives = imge::list(self.args.all_drives);
			self.selected_row = 0;

			for i in 0..self.drives.len() {
				if self.selected_name == self.drives[i].name {
					self.selected_row = i;
					break;
				}
			}

			if !self.drives.is_empty() && self.selected_row > self.drives.len() - 1 {
				self.selected_row = self.drives.len() - 1;
			}

			if self.drives.is_empty() {
				self.selected_name = "".to_string();
				self.selected_size = 0;
				return;
			}
		}

		self.selected_name.clone_from(&self.drives[self.selected_row].name);
		self.selected_size = self.drives[self.selected_row].size;
	}
}
