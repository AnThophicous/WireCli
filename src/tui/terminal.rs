use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout, Write};

const ENABLE_ALTERNATE_SCROLL: &str = "\x1b[?1007h";
const DISABLE_ALTERNATE_SCROLL: &str = "\x1b[?1007l";

pub(super) fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, String> {
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| e.to_string())?;
    set_alternate_scroll(&mut stdout, true)?;
    execute!(stdout, EnableBracketedPaste).map_err(|e| e.to_string())?;
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|e| e.to_string())
}

pub(super) fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<(), String> {
    let mut first_error = None;
    record_restore_result(
        &mut first_error,
        disable_raw_mode().map_err(|e| e.to_string()),
    );
    record_restore_result(
        &mut first_error,
        set_alternate_scroll(terminal.backend_mut(), false),
    );
    record_restore_result(
        &mut first_error,
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags).map_err(|e| e.to_string()),
    );
    record_restore_result(
        &mut first_error,
        execute!(terminal.backend_mut(), DisableBracketedPaste).map_err(|e| e.to_string()),
    );
    record_restore_result(
        &mut first_error,
        execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|e| e.to_string()),
    );
    record_restore_result(
        &mut first_error,
        terminal.show_cursor().map_err(|e| e.to_string()),
    );
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn set_alternate_scroll<W: Write>(writer: &mut W, enabled: bool) -> Result<(), String> {
    let sequence = if enabled {
        ENABLE_ALTERNATE_SCROLL
    } else {
        DISABLE_ALTERNATE_SCROLL
    };
    writer
        .write_all(sequence.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())
}

fn record_restore_result(first_error: &mut Option<String>, result: Result<(), String>) {
    if first_error.is_none() {
        if let Err(err) = result {
            *first_error = Some(err);
        }
    }
}
