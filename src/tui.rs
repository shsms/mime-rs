//! `mime tui SCRIPT.tl --file F` — the script stepper: watch a tulisp script
//! land form by form. Three panes over a [`crate::tui_step::Stepper`] — the
//! buffer viewport (around point), the program (top-level forms, next one
//! highlighted), the last step's report (diff + reports/log, or the error) —
//! plus a status bar. Playback: SPACE/n = one form, p = auto-play (\[/\] =
//! slower/faster), b = back one form, r = restart, f = run to the end —
//! b/r are refused once a form has done file I/O (a replay would re-run it
//! against changed files). Quitting: w = write the finished PRIMARY buffer
//! (the one the tui opened) and quit; q = quit discarding (unless `--write`
//! was given AND the script ran to completion).

use std::io::Write as _;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui_step::Stepper;

/// Run the stepper UI. `write_back` persists the buffer to `file` on quit —
/// only when every form has run (a half-stepped script never writes).
pub fn run(
    script: &std::path::Path,
    file: &std::path::Path,
    write_back: bool,
) -> Result<(), String> {
    let source = std::fs::read_to_string(script)
        .map_err(|e| format!("cannot read program {}: {e}", script.display()))?;
    let path = crate::safety::check_path(file)?;
    let store: Box<dyn crate::TextStore> = Box::new(
        crate::Quire::open(&path)
            .map_err(|e| format!("cannot open file {}: {e}", path.display()))?,
    );
    let mut stepper = Stepper::new(store, &source, Vec::new())?;

    let mut terminal =
        ratatui::try_init().map_err(|e| format!("cannot set up the terminal: {e}"))?;
    let run = ui_loop(&mut terminal, &mut stepper);
    ratatui::restore();
    let quit = run?;

    if write_back || quit == Quit::Write {
        if stepper.finished() {
            // The PRIMARY buffer — a script ending on another buffer must
            // not clobber `file` with that buffer's content.
            let text = stepper.primary_text();
            crate::safety::write_atomic(&path, text.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            println!("wrote {} ({} bytes)", path.display(), text.len());
        } else {
            eprintln!(
                "note: quit after step {}/{} — nothing written (run to the end for --write)",
                stepper.next,
                stepper.forms.len()
            );
        }
    } else {
        println!("discarded (pass --write, or press w after a completed run, to persist)");
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

/// How the user left the UI: `w` asks for the buffer to be written (the
/// interactive counterpart of `--write`), `q`/Esc discards.
#[derive(PartialEq)]
enum Quit {
    Write,
    Discard,
}

/// Auto-play state: whether the script is playing, the delay per form, and
/// a transient notice (a refused replay) shown in the status bar until the
/// next action.
struct Playback {
    playing: bool,
    delay: Duration,
    notice: Option<&'static str>,
}

const MIN_DELAY: Duration = Duration::from_millis(25);
const MAX_DELAY: Duration = Duration::from_millis(4000);

fn ui_loop(terminal: &mut ratatui::DefaultTerminal, stepper: &mut Stepper) -> Result<Quit, String> {
    let mut pb = Playback {
        playing: false,
        delay: Duration::from_millis(500),
        notice: None,
    };
    loop {
        if stepper.finished() {
            pb.playing = false;
        }
        terminal
            .draw(|frame| draw(frame, stepper, &pb))
            .map_err(|e| format!("draw failed: {e}"))?;
        // Playing: wait at most one per-form delay for a key, then step.
        // Paused: block until a key arrives (poll with a long timeout, so a
        // resize still redraws).
        let timeout = if pb.playing {
            pb.delay
        } else {
            Duration::from_secs(3600)
        };
        if !event::poll(timeout).map_err(|e| format!("input failed: {e}"))? {
            if pb.playing {
                stepper.step();
            }
            continue;
        }
        match event::read().map_err(|e| format!("input failed: {e}"))? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Quit::Discard),
                // Write-and-quit — only a COMPLETED run may write (the same
                // rule as --write); ignored mid-script.
                KeyCode::Char('w') if stepper.finished() => return Ok(Quit::Write),
                KeyCode::Char(' ') | KeyCode::Char('n') | KeyCode::Enter => {
                    pb.playing = false;
                    pb.notice = None;
                    stepper.step();
                }
                KeyCode::Char('p') => {
                    pb.playing = !pb.playing && !stepper.finished();
                    pb.notice = None;
                }
                KeyCode::Char('b') => {
                    pb.playing = false;
                    pb.notice = stepper.step_back().err();
                }
                KeyCode::Char('r') => {
                    pb.playing = false;
                    pb.notice = stepper.restart().err();
                }
                KeyCode::Char('f') => {
                    pb.playing = false;
                    pb.notice = None;
                    while stepper.step().is_some() {}
                }
                KeyCode::Char(']') => pb.delay = (pb.delay / 2).max(MIN_DELAY),
                KeyCode::Char('[') => pb.delay = (pb.delay * 2).min(MAX_DELAY),
                _ => {}
            },
            _ => {}
        }
    }
}

fn draw(frame: &mut ratatui::Frame, stepper: &Stepper, pb: &Playback) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(frame.area());
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(cols[1]);

    draw_buffer(frame, cols[0], stepper);
    draw_program(frame, right[0], stepper);
    draw_report(frame, right[1], stepper);
    draw_status(frame, rows[1], stepper, pb);
}

/// The status bar: workspace state + playback state, and the key map.
fn draw_status(frame: &mut ratatui::Frame, area: Rect, stepper: &Stepper, pb: &Playback) {
    let play = if pb.playing {
        format!("▶ playing {}ms/form", pb.delay.as_millis())
    } else if stepper.finished() {
        "■ finished".to_string()
    } else {
        format!("‖ paused {}ms/form", pb.delay.as_millis())
    };
    let status = format!(
        " {} — point {} — step {}/{} — {}",
        stepper.status_line(),
        stepper.point,
        stepper.next,
        stepper.forms.len(),
        play,
    );
    let keys = if stepper.finished() {
        " w write+quit · b back · r restart · q discard"
    } else {
        " SPACE step · p play/pause · [/] speed · b back · r restart · f finish · q quit"
    };
    // A refused action replaces the key hints until the next action — the
    // same line, so the layout never jumps.
    let second = match pb.notice {
        Some(n) => Line::from(Span::styled(
            format!(" ⚠ {n}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        None => Line::from(Span::styled(
            keys,
            Style::default().add_modifier(Modifier::DIM),
        )),
    };
    let lines = vec![
        Line::from(Span::styled(
            status,
            Style::default().add_modifier(Modifier::REVERSED),
        )),
        second,
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

/// The buffer viewport: the lines around point, the point line highlighted.
fn draw_buffer(frame: &mut ratatui::Frame, area: Rect, stepper: &Stepper) {
    let text = stepper.text();
    let height = area.height.saturating_sub(2) as usize; // borders
    // Point (a 1-based char position) → its line index.
    let mut seen = 0usize;
    let mut point_line = 0usize;
    for (i, l) in text.split('\n').enumerate() {
        let len = l.chars().count() + 1; // + the newline
        if stepper.point <= seen + len {
            point_line = i;
            break;
        }
        seen += len;
        point_line = i;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let first = point_line.saturating_sub(height / 2);
    let rendered: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, l)| {
            let n = format!("{:>5} ", i + 1);
            let style = if i == point_line {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(n, Style::default().add_modifier(Modifier::DIM)),
                Span::styled((*l).to_string(), style),
            ])
        })
        .collect();
    let title = format!(
        " buffer — point {} (line {}) — step {}/{} ",
        stepper.point,
        point_line + 1,
        stepper.next,
        stepper.forms.len()
    );
    frame.render_widget(
        Paragraph::new(rendered).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// The program pane: one entry per top-level form, the next one highlighted.
fn draw_program(frame: &mut ratatui::Frame, area: Rect, stepper: &Stepper) {
    let height = area.height.saturating_sub(2) as usize;
    let first = stepper.next.saturating_sub(height / 2);
    let rendered: Vec<Line> = stepper
        .forms
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, f)| {
            let head = f.text.split('\n').next().unwrap_or("");
            let marker = if i == stepper.next { "▶ " } else { "  " };
            let style = if i == stepper.next {
                Style::default().add_modifier(Modifier::BOLD)
            } else if i < stepper.next {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!("{marker}L{:<4} {head}", f.line),
                style,
            ))
        })
        .collect();
    let title = if stepper.finished() {
        " program — finished (w to write, q to discard) "
    } else {
        " program — SPACE to eval the next form "
    };
    frame.render_widget(
        Paragraph::new(rendered).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// The report pane: what the last step did (its diff and reports/log), or the
/// error it died with.
fn draw_report(frame: &mut ratatui::Frame, area: Rect, stepper: &Stepper) {
    let mut lines: Vec<Line> = Vec::new();
    match &stepper.last {
        None => lines.push(Line::from("no step yet — SPACE to begin")),
        Some(out) => {
            if let Some(e) = &out.error {
                lines.push(Line::from(Span::styled(
                    format!("step {} FAILED (edits rolled back):", out.index + 1),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                for l in e.lines() {
                    lines.push(Line::from(l.to_string()));
                }
            } else if out.dirty {
                for l in out.diff.lines() {
                    lines.push(Line::from(l.to_string()));
                }
            } else {
                lines.push(Line::from(format!(
                    "step {} — clean (no edit)",
                    out.index + 1
                )));
            }
            for l in &out.log {
                lines.push(Line::from(format!("log: {l}")));
            }
            for (k, v) in &out.reports {
                lines.push(Line::from(format!("{k}: {v}")));
            }
        }
    }
    let height = area.height.saturating_sub(2) as usize;
    if lines.len() > height {
        lines.truncate(height.saturating_sub(1));
        lines.push(Line::from("…"));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" last step ")),
        area,
    );
}
