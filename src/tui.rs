//! `mime tui SCRIPT.tl --file F` — the Phase-0 script stepper: watch a tulisp
//! script land form by form. Three panes over a [`crate::tui_step::Stepper`]:
//! the buffer viewport (around point), the program (top-level forms, next one
//! highlighted), and the last step's report (diff + reports/log, or the
//! error). SPACE/n = eval the next form, q = quit — discarding unless
//! `--write` was given AND the script ran to completion.

use std::io::Write as _;

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
    run?;

    if write_back {
        if stepper.finished() {
            let text = stepper.text();
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
        println!("discarded (pass --write to persist a completed run)");
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

fn ui_loop(terminal: &mut ratatui::DefaultTerminal, stepper: &mut Stepper) -> Result<(), String> {
    loop {
        terminal
            .draw(|frame| draw(frame, stepper))
            .map_err(|e| format!("draw failed: {e}"))?;
        match event::read().map_err(|e| format!("input failed: {e}"))? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char(' ') | KeyCode::Char('n') | KeyCode::Enter => {
                    stepper.step();
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn draw(frame: &mut ratatui::Frame, stepper: &Stepper) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(frame.area());
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(cols[1]);

    draw_buffer(frame, cols[0], stepper);
    draw_program(frame, right[0], stepper);
    draw_report(frame, right[1], stepper);
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
        " program — finished (q to quit) "
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
