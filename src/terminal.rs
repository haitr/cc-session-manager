//! Default host: full-screen terminal (alternate screen).
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::app::Model;
use crate::view;

pub fn run(mut model: Model) -> Result<()> {
    // init() enters the alternate screen + raw mode and installs a panic hook that restores both.
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut model);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, model: &mut Model) -> Result<()> {
    let mut last = Instant::now();
    while !model.should_quit {
        model.apply();
        terminal.draw(|frame| view::view(frame, model))?;

        let now = Instant::now();
        model.spinner_state.tick(now - last);
        last = now;

        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Raw mode swallows the signal, so Ctrl+C arrives as a key.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                break;
            }
            model.handle(key.code);
        }
    }
    Ok(())
}
