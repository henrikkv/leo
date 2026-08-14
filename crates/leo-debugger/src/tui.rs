// Copyright (C) 2019-2026 Provable Inc.
// This file is part of the Leo library.

// The Leo library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The Leo library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the Leo library. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    DebugCommand,
    breakpoints::{BreakpointKey, ReverseIndex, resolve_file_index},
    hook::{Context, DebugEvent, Phase},
    session::{ProgramSource, Session},
};

use leo_passes::ProgramDebugMap;
use snarkvm_synthesizer_process::Frame as StackFrame;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use anyhow::Result;
use std::{collections::HashMap, io, time::Duration};

struct ProgramView {
    bytecode_lines: Vec<String>,
    debug_info: Option<ProgramDebugMap>,
    source_cache: HashMap<usize, Vec<String>>,
}

struct StopInfo {
    program: String,
    function: String,
    context: Context,
    index: usize,
    frames: Vec<StackFrame>,
    loc: Option<crate::hook::SourceLoc>,
    registers: Vec<(u64, String)>,
}

impl From<crate::hook::Position> for StopInfo {
    fn from(position: crate::hook::Position) -> Self {
        Self {
            program: position.program,
            function: position.function,
            context: position.context,
            index: position.index,
            frames: position.frames,
            loc: position.loc,
            registers: position.registers,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pane {
    Source,
    Bytecode,
    Console,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Self::Source => Self::Bytecode,
            Self::Bytecode => Self::Console,
            Self::Console => Self::Source,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Source => Self::Console,
            Self::Bytecode => Self::Source,
            Self::Console => Self::Bytecode,
        }
    }
}

struct App {
    programs: HashMap<String, ProgramView>,
    console: Vec<String>,
    input: String,
    last_stop: Option<StopInfo>,
    finished: bool,
    quit: bool,
    focus: Pane,
    source_scroll: u16,
    bytecode_scroll: u16,
    console_scroll: u16,
    source_follow: bool,
    bytecode_follow: bool,
    console_follow: bool,
    source_height: u16,
    bytecode_height: u16,
    console_height: u16,
    source_len: usize,
    bytecode_len: usize,
}

impl App {
    fn program_view_mut(&mut self, program: &str, file: usize) -> Option<&[String]> {
        let view = self.programs.get_mut(program)?;
        if !view.source_cache.contains_key(&file) {
            let path = view.debug_info.as_ref()?.source_files.get(file)?.clone();
            let lines = std::fs::read_to_string(&path)
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_else(|e| vec![format!("(failed to read {path}: {e})")]);
            view.source_cache.insert(file, lines);
        }
        view.source_cache.get(&file).map(Vec::as_slice)
    }
}

pub fn run(session: Session, programs: Vec<ProgramSource>) -> Result<()> {
    let mut program_views = HashMap::new();
    for program in programs {
        program_views.insert(program.name.clone(), ProgramView {
            bytecode_lines: program.bytecode.lines().map(str::to_string).collect(),
            debug_info: program.debug_info,
            source_cache: HashMap::new(),
        });
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        programs: program_views,
        console: vec![
            "leo debug — type 'help' for commands.".to_string(),
            "Running to the first breakpoint...".to_string(),
        ],
        input: String::new(),
        last_stop: None,
        finished: false,
        quit: false,
        focus: Pane::Console,
        source_scroll: 0,
        bytecode_scroll: 0,
        console_scroll: 0,
        source_follow: true,
        bytecode_follow: true,
        console_follow: true,
        source_height: 1,
        bytecode_height: 1,
        console_height: 1,
        source_len: 0,
        bytecode_len: 0,
    };

    let result = run_loop(&mut terminal, &session, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let _ = session.cmd_tx.send(DebugCommand::Quit);
    let _ = session.handle.join();

    result
}

fn run_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    session: &Session,
    app: &mut App,
) -> Result<()> {
    loop {
        while let Ok(event) = session.evt_rx.try_recv() {
            handle_event(app, event);
        }

        terminal.draw(|frame| draw(frame, app))?;

        if app.quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('j') if ctrl => app.focus = app.focus.next(),
                KeyCode::Char('k') if ctrl => app.focus = app.focus.prev(),
                KeyCode::Char('u') if ctrl => scroll_focused(app, ScrollAmount::HalfPageUp),
                KeyCode::Char('d') if ctrl => scroll_focused(app, ScrollAmount::HalfPageDown),
                KeyCode::Char('j') if !ctrl && app.focus != Pane::Console => {
                    scroll_focused(app, ScrollAmount::LineDown);
                }
                KeyCode::Char('k') if !ctrl && app.focus != Pane::Console => {
                    scroll_focused(app, ScrollAmount::LineUp);
                }
                KeyCode::Enter if app.focus == Pane::Console => {
                    let line = std::mem::take(&mut app.input);
                    if !line.trim().is_empty() {
                        app.console.push(format!("(leo-debug) {line}"));
                        app.console_follow = true;
                        handle_command(&line, session, app);
                    }
                }
                KeyCode::Backspace if app.focus == Pane::Console => {
                    app.input.pop();
                }
                KeyCode::Char(c) if app.focus == Pane::Console && !ctrl => app.input.push(c),
                KeyCode::Esc => match app.focus {
                    Pane::Source => app.source_follow = true,
                    Pane::Bytecode => app.bytecode_follow = true,
                    Pane::Console => app.console_follow = true,
                },
                _ => {}
            }
        }
    }
}

fn handle_event(app: &mut App, event: DebugEvent) {
    match event {
        DebugEvent::Stopped(position) => {
            let loc_str = position.loc.map(|l| format!(" ({}:{})", l.line, l.col)).unwrap_or_default();
            app.console.push(format!(
                "(depth {}) {}::{} [{}] {}{loc_str}",
                position.depth, position.program, position.function, position.index, position.text
            ));
            app.last_stop = Some(position.into());
            app.source_follow = true;
            app.bytecode_follow = true;
        }
        DebugEvent::Progress(position) => {
            app.last_stop = Some(position.into());
            app.source_follow = true;
            app.bytecode_follow = true;
        }
        DebugEvent::PhaseChange(Phase::Finalize) => {
            app.console.push("--- entering finalize phase ---".to_string());
        }
        DebugEvent::PhaseChange(Phase::Transition) => {}
        DebugEvent::Finished => {
            app.console.push("✅ Finished.".to_string());
            app.finished = true;
        }
        DebugEvent::Halted { reason } => {
            app.console.push(format!("❌ Halted: {reason}"));
            app.finished = true;
        }
    }
}

enum ScrollAmount {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
}

fn scroll_focused(app: &mut App, amount: ScrollAmount) {
    let focus = app.focus;
    let (scroll, follow, height, len) = match focus {
        Pane::Source => (&mut app.source_scroll, &mut app.source_follow, app.source_height, app.source_len),
        Pane::Bytecode => (&mut app.bytecode_scroll, &mut app.bytecode_follow, app.bytecode_height, app.bytecode_len),
        Pane::Console => (&mut app.console_scroll, &mut app.console_follow, app.console_height, app.console.len()),
    };
    *follow = false;
    let step = match amount {
        ScrollAmount::LineUp | ScrollAmount::LineDown => 1u16,
        ScrollAmount::HalfPageUp | ScrollAmount::HalfPageDown => (height / 2).max(1),
    };
    match amount {
        ScrollAmount::LineUp | ScrollAmount::HalfPageUp => {
            *scroll = scroll.saturating_sub(step);
        }
        ScrollAmount::LineDown | ScrollAmount::HalfPageDown => {
            *scroll = scroll.saturating_add(step);
        }
    }
    let max_scroll = len.saturating_sub(height as usize) as u16;
    *scroll = (*scroll).min(max_scroll);
    if focus == Pane::Console && *scroll >= max_scroll {
        *follow = true;
    }
}

fn handle_command(line: &str, session: &Session, app: &mut App) {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else { return };
    let rest: Vec<&str> = parts.collect();

    match cmd {
        "help" | "h" => {
            app.console.push(
                "break|b file:line, continue|c, step|s, next|n, stepi|si, nexti|ni, print|p <reg>, \
                 backtrace|bt, quit|q | keys: C-j/C-k focus, C-u/C-d half-page, j/k line, Esc follow"
                    .to_string(),
            );
        }
        "continue" | "c" => send(session, app, DebugCommand::Continue),
        "step" | "s" => send(session, app, DebugCommand::StepInto),
        "next" | "n" => send(session, app, DebugCommand::StepOverLine),
        "stepi" | "si" => send(session, app, DebugCommand::StepInstr),
        "nexti" | "ni" => send(session, app, DebugCommand::NextInstr),
        "quit" | "q" => {
            let _ = session.cmd_tx.send(DebugCommand::Quit);
            app.quit = true;
        }
        "break" | "b" => {
            let Some(spec) = rest.first() else {
                app.console.push("usage: break file:line | break program.aleo:file:line".to_string());
                return;
            };
            match resolve_breakpoint(spec, &app.programs) {
                Ok(key) => {
                    let described = format!("{}::{} [{}]", key.program, key.function, key.index);
                    session.state.lock().expect("hook state mutex poisoned").breakpoints.insert(key);
                    app.console.push(format!("Breakpoint set at {described}."));
                }
                Err(message) => app.console.push(message),
            }
        }
        "backtrace" | "bt" => {
            let Some(stop) = &app.last_stop else {
                app.console.push("Not currently stopped.".to_string());
                return;
            };
            for (i, frame) in stop.frames.iter().rev().enumerate() {
                let dynamic = if frame.is_dynamic { " (dynamic)" } else { "" };
                app.console.push(format!("#{i} {}::{}{dynamic}", frame.program_id, frame.function_name));
            }
            app.console.push(format!("#{} {}::{} (current)", stop.frames.len(), stop.program, stop.function));
        }
        "print" | "p" => {
            let Some(stop) = &app.last_stop else {
                app.console.push("Not currently stopped.".to_string());
                return;
            };
            match rest.first() {
                Some(reg) if reg.starts_with('r') && reg[1..].parse::<u64>().is_ok() => {
                    let index: u64 = reg[1..].parse().unwrap();
                    match stop.registers.iter().find(|(r, _)| *r == index) {
                        Some((_, value)) => app.console.push(format!("r{index} = {value}")),
                        None => app.console.push(format!("r{index} is not assigned yet.")),
                    }
                }
                Some(_) | None => {
                    for (register, value) in &stop.registers {
                        app.console.push(format!("r{register} = {value}"));
                    }
                }
            }
        }
        other => app.console.push(format!("Unknown command '{other}'. Type 'help' for a list.")),
    }
}

fn send(session: &Session, app: &mut App, command: DebugCommand) {
    if app.finished {
        app.console.push("Session already finished.".to_string());
        return;
    }
    if session.cmd_tx.send(command).is_err() {
        app.console.push("Session's command channel is closed.".to_string());
    }
}

fn resolve_breakpoint(
    spec: &str,
    programs: &HashMap<String, ProgramView>,
) -> std::result::Result<BreakpointKey, String> {
    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    let (program_filter, file, line) = match parts.as_slice() {
        [file, line] => (None, *file, *line),
        [program, file, line] => (Some(*program), *file, *line),
        _ => return Err(format!("Invalid breakpoint '{spec}': expected 'file:line' or 'program.aleo:file:line'")),
    };
    let line: u32 = line.parse().map_err(|_| format!("Invalid line number in breakpoint '{spec}'"))?;

    for (name, view) in programs {
        if program_filter.is_some_and(|filter| filter != name) {
            continue;
        }
        let Some(debug_info) = &view.debug_info else { continue };
        let Some(file_index) = resolve_file_index(debug_info, file) else { continue };
        let index = ReverseIndex::build(debug_info);
        if let Some((function, context, instruction_index, _line)) = index.resolve(file_index, line) {
            return Ok(BreakpointKey { program: name.clone(), function, context, index: instruction_index });
        }
    }
    Err(format!("Could not resolve breakpoint '{spec}' against any loaded program's debug info"))
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(25), Constraint::Percentage(50)])
        .split(frame.area());

    draw_source_pane(frame, chunks[0], app);
    draw_bytecode_pane(frame, chunks[1], app);
    draw_console_pane(frame, chunks[2], app);
}

const HIGHLIGHT: Style = Style::new().bg(Color::Rgb(60, 60, 60)).fg(Color::White).add_modifier(Modifier::BOLD);
const TITLE_BAR: Style = Style::new().bg(Color::LightGreen).fg(Color::Black);
const TITLE_BAR_FOCUSED: Style = Style::new().bg(Color::Green).fg(Color::Black).add_modifier(Modifier::BOLD);

fn split_title_bar(area: Rect) -> (Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    (chunks[0], chunks[1])
}

fn draw_title_bar(frame: &mut ratatui::Frame, area: Rect, title: &str, focused: bool) {
    let style = if focused { TITLE_BAR_FOCUSED } else { TITLE_BAR };
    let marker = if focused { "▸" } else { " " };
    frame.render_widget(Paragraph::new(format!("{marker} {title}")).style(style), area);
}

fn clamp_scroll(scroll: u16, len: usize, height: u16) -> u16 {
    let max_scroll = len.saturating_sub(height as usize) as u16;
    scroll.min(max_scroll)
}

fn source_file_label(app: &App, program: &str, file: usize) -> Option<String> {
    let path = app.programs.get(program)?.debug_info.as_ref()?.source_files.get(file)?;
    Some(
        std::path::Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone()),
    )
}

fn draw_source_pane(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let (bar, content) = split_title_bar(area);
    let focused = app.focus == Pane::Source;
    app.source_height = content.height;

    let Some(stop) = &app.last_stop else {
        draw_title_bar(frame, bar, "Leo source", focused);
        app.source_len = 1;
        frame.render_widget(Paragraph::new("(not running)"), content);
        return;
    };
    let (program, loc) = (stop.program.clone(), stop.loc);
    let Some(loc) = loc else {
        draw_title_bar(frame, bar, "Leo source", focused);
        let text = format!("(no source location for {program}::{} — synthetic scaffolding)", stop.function);
        app.source_len = 1;
        frame.render_widget(Paragraph::new(text), content);
        return;
    };
    let file = loc.file;
    let title = source_file_label(app, &program, file).unwrap_or_else(|| "Leo source".to_string());
    draw_title_bar(frame, bar, &title, focused);

    let highlight_line = loc.line;
    let lines = app.program_view_mut(&program, file).map(<[String]>::to_vec).unwrap_or_default();
    app.source_len = lines.len();
    if app.source_follow {
        app.source_scroll = (highlight_line.saturating_sub(1)).saturating_sub(content.height as u32 / 2) as u16;
    }
    app.source_scroll = clamp_scroll(app.source_scroll, lines.len(), content.height);

    let text: Vec<Line> = lines
        .iter()
        .enumerate()
        .map(|(i, line_content)| {
            let line_number = (i + 1) as u32;
            let rendered = format!("{line_number:>5} | {line_content}");
            if line_number == highlight_line {
                Line::from(Span::styled(rendered, HIGHLIGHT))
            } else {
                Line::from(rendered)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(text).scroll((app.source_scroll, 0)), content);
}

fn draw_bytecode_pane(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let (bar, content) = split_title_bar(area);
    let focused = app.focus == Pane::Bytecode;
    app.bytecode_height = content.height;

    let Some(stop) = &app.last_stop else {
        draw_title_bar(frame, bar, "Aleo bytecode", focused);
        app.bytecode_len = 1;
        frame.render_widget(Paragraph::new("(not running)"), content);
        return;
    };
    draw_title_bar(frame, bar, &stop.program, focused);

    let Some(view) = app.programs.get(&stop.program) else {
        app.bytecode_len = 1;
        frame.render_widget(Paragraph::new("(program not loaded)"), content);
        return;
    };
    let context_label = match stop.context {
        Context::Transition => "transition",
        Context::Finalize => "finalize",
    };
    let header = format!("{}::{}  [{} #{}]", stop.program, stop.function, context_label, stop.index);
    let highlight_line = locate_bytecode_line(&view.bytecode_lines, &stop.function, stop.context, stop.index);
    let text_len = view.bytecode_lines.len() + 1;
    app.bytecode_len = text_len;
    if app.bytecode_follow {
        let target = highlight_line.map(|i| i + 1).unwrap_or(0);
        app.bytecode_scroll = (target as u32).saturating_sub(content.height as u32 / 2) as u16;
    }
    app.bytecode_scroll = clamp_scroll(app.bytecode_scroll, text_len, content.height);

    let text: Vec<Line> =
        std::iter::once(Line::from(Span::styled(header, Style::new().add_modifier(Modifier::ITALIC))))
            .chain(view.bytecode_lines.iter().enumerate().map(|(i, line)| {
                if Some(i) == highlight_line {
                    Line::from(Span::styled(line.as_str(), HIGHLIGHT))
                } else {
                    Line::from(line.as_str())
                }
            }))
            .collect();
    frame.render_widget(Paragraph::new(text).scroll((app.bytecode_scroll, 0)), content);
}

fn locate_bytecode_line(lines: &[String], function: &str, context: Context, index: usize) -> Option<usize> {
    let header_prefixes: &[String] = &match context {
        Context::Transition => {
            vec![format!("function {function}:"), format!("closure {function}:")]
        }
        Context::Finalize if function == "constructor" => vec!["constructor:".to_string()],
        Context::Finalize => vec![format!("finalize {function}:")],
    };
    let start = lines.iter().position(|line| header_prefixes.iter().any(|prefix| line.trim() == prefix))? + 1;

    let mut seen = 0;
    for (offset, line) in lines[start..].iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) && trimmed.ends_with(':') {
            return None;
        }
        if trimmed.starts_with("input ") || trimmed.starts_with("output ") {
            continue;
        }
        if seen == index {
            return Some(start + offset);
        }
        seen += 1;
    }
    None
}

fn draw_console_pane(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let (bar, body) = split_title_bar(area);
    draw_title_bar(frame, bar, "Console (help for commands)", app.focus == Pane::Console);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(body);
    let log_area = chunks[0];
    let input_area = chunks[1];

    app.console_height = log_area.height;
    let max_scroll = app.console.len().saturating_sub(log_area.height as usize) as u16;
    if app.console_follow {
        app.console_scroll = max_scroll;
    } else {
        app.console_scroll = clamp_scroll(app.console_scroll, app.console.len(), log_area.height);
    }

    let start = app.console_scroll as usize;
    let end = (start + log_area.height as usize).min(app.console.len());
    let lines: Vec<Line> = app.console[start..end].iter().map(|line| Line::from(line.as_str())).collect();
    frame.render_widget(Paragraph::new(lines), log_area);
    frame.render_widget(Paragraph::new(format!("(leo-debug) {}", app.input)), input_area);
}
