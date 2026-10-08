//! Rendering: state to characters. Pure — the same function serves the
//! tests' `TestBackend` and the live terminal.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::state::{Line, Modal, TuiState};

/// Render one full frame.
pub fn render(frame: &mut Frame, state: &TuiState) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    render_header(frame, chunks[0], state);
    render_transcript(frame, chunks[1], state);
    render_input(frame, chunks[2], state);
    render_status(frame, chunks[3], state);

    match state.modal {
        Modal::None => {}
        Modal::Help => render_help(frame, frame.area()),
        Modal::Sessions => render_sessions(frame, frame.area(), state),
    }

    if let Some(pending) = &state.pending_approval {
        render_approval(frame, frame.area(), pending);
    }
}

fn render_header(frame: &mut Frame, area: Rect, state: &TuiState) {
    let model = state.model.as_deref().unwrap_or("failover-kette");
    let left = Span::styled(
        format!(" bkgclaw · {} · policy {}", model, state.policy),
        Style::default().fg(Color::Cyan),
    );
    let right = Span::styled(
        format!("${:.4} · {} ", state.spent, state.session),
        Style::default().fg(Color::DarkGray),
    );
    let width = area.width as usize;
    let left_text = left.content.clone();
    let pad = width.saturating_sub(left_text.chars().count() + right.content.chars().count() + 2);
    let line = TextLine::from(vec![left, Span::raw(" ".repeat(pad)), right]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Rgb(24, 28, 34)).fg(Color::White)),
        area,
    );
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &TuiState) {
    let mut lines: Vec<TextLine> = Vec::new();
    for line in &state.lines {
        lines.push(match line {
            Line::User(text) => TextLine::from(vec![
                Span::styled("du          ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    clip_start(text, (area.width.saturating_sub(14)) as usize),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::Assistant(text) => TextLine::from(vec![
                Span::styled("agent       ", Style::default().fg(Color::Green)),
                Span::styled(
                    clip_start(text, (area.width.saturating_sub(14)) as usize),
                    Style::default(),
                ),
            ]),
            Line::Reasoning(text) => TextLine::from(vec![
                Span::styled("denken      ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    clip_start(text, (area.width.saturating_sub(14)) as usize),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::Tool {
                name,
                outcome,
                output,
            } => {
                let (color, marker) = match outcome.as_str() {
                    "ran" => (Color::Green, "⚙"),
                    "denied" => (Color::Yellow, "⊘"),
                    _ => (Color::Red, "✗"),
                };
                TextLine::from(vec![
                    Span::styled("  werkzeug  ", Style::default().fg(Color::Blue)),
                    Span::styled(format!("{marker} {name} "), Style::default().fg(color)),
                    Span::styled(clip_start(output, 80), Style::default().fg(Color::DarkGray)),
                ])
            }
            Line::Status(text) => TextLine::from(Span::styled(
                format!("  {text}"),
                Style::default().fg(Color::Blue),
            )),
            Line::Error(text) => TextLine::from(vec![
                Span::styled("  fehler    ", Style::default().fg(Color::Red)),
                Span::styled(text, Style::default().fg(Color::Red)),
            ]),
            Line::Cancelled => TextLine::from(Span::styled(
                "  ⏹ turn abgebrochen",
                Style::default().fg(Color::Yellow),
            )),
        });
    }

    // The live buffers appear as soon as their first delta arrives, so the
    // operator sees thinking as it happens.
    if !state.live_reasoning.is_empty() {
        lines.push(TextLine::from(vec![
            Span::styled("denken      ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                state.live_reasoning.clone(),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    if !state.live.is_empty() {
        lines.push(TextLine::from(vec![
            Span::styled("agent       ", Style::default().fg(Color::Green)),
            Span::styled(state.live.clone(), Style::default()),
        ]));
    }

    let viewport = area.height.saturating_sub(2) as usize; // minus borders
    let total = lines.len();
    let skip = total.saturating_sub(viewport + state.scroll as usize);

    let paragraph = Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(50, 56, 66))),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// Newline the input box understands: wrap, and a hint when a turn runs.
fn render_input(frame: &mut Frame, area: Rect, state: &TuiState) {
    let title = if state.busy {
        " eingabe (ctrl-c bricht den turn ab) "
    } else {
        " eingabe (enter sendet, ctrl+j neue zeile, F1 hilfe) "
    };
    let input = Paragraph::new(state.input.clone())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Rgb(50, 56, 66))),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
}

fn render_status(frame: &mut Frame, area: Rect, state: &TuiState) {
    let colour = if state.busy {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let text = format!(" {}", state.status);
    let line = TextLine::from(Span::styled(
        text,
        Style::default().fg(colour).add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 50, 16);
    let items = vec![
        "enter       senden · ctrl+j  neue zeile",
        "ctrl-c      turn abbrechen / beenden",
        "ctrl-q      beenden",
        "hoch/runter im transkript scrollen",
        "F1          diese hilfe",
        "F2          sitzungen",
        "/new        neue sitzung",
        "/fork <id> [n]  sitzung gabeln",
        "/model <id> modell pinnen",
        "/policy <p> policy setzen",
        "/cost       ausgaben der sitzung",
        "",
        "freigaben:  j = ja · n = nein · i = immer",
    ];
    let list = List::new(
        items
            .into_iter()
            .map(|i| {
                ListItem::new(TextLine::from(Span::styled(
                    i,
                    Style::default().fg(Color::White),
                )))
            })
            .collect::<Vec<_>>(),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" hilfe ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(list, popup);
}

fn render_sessions(frame: &mut Frame, area: Rect, state: &TuiState) {
    let popup = centered(area, 60, 20);
    let items: Vec<ListItem> = state
        .sessions
        .iter()
        .map(|(id, preview)| {
            let active = *id == state.session;
            let style = if active {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(TextLine::from(Span::styled(
                format!("{id}  {preview}"),
                style,
            )))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" sitzungen (F2 schließt) ")
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut list_state = ListState::default();
    list_state.select(None);
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut list_state);
}

fn render_approval(frame: &mut Frame, area: Rect, pending: &crate::state::PendingApproval) {
    let popup = centered(area, 60, 8);
    let lines = vec![
        TextLine::from(Span::styled(
            " Freigabe nötig",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        TextLine::from(vec![
            Span::styled("  werkzeug: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                pending.name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({})", pending.risk),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        TextLine::from(vec![
            Span::styled("  argumente: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                clip_start(&pending.arguments, 70),
                Style::default().fg(Color::White),
            ),
        ]),
        TextLine::from(""),
        TextLine::from(Span::styled(
            "  [j] erlauben · [n] ablehnen · [i] immer erlauben",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    let block = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
}

fn centered(area: Rect, percent_x: u16, height: u16) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let popup_x = area.x + (area.width - popup_width) / 2;
    let popup_height = height.min(area.height.saturating_sub(2));
    let popup_y = area.y + (area.height - popup_height) / 2;
    Rect::new(popup_x, popup_y, popup_width, popup_height)
}

/// Multiline text to one clipped line, keeping the first row: the
/// transcript grows downward, each message's first line carries it.
fn clip_start(text: &str, width: usize) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if first.chars().count() <= width {
        first.to_string()
    } else {
        format!(
            "{}…",
            first
                .chars()
                .take(width.saturating_sub(1))
                .collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TuiState;

    /// Render into a TestBackend and return the screen as text.
    fn render_to_text(width: u16, height: u16, state: &TuiState) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render(frame, state))
            .expect("frame renders");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn a_message_reaches_the_screen() {
        let mut state = TuiState::new("s-1".into(), None, "allow-read-only".into());
        state
            .lines
            .push(crate::state::Line::User("schreibe eine app".into()));
        state
            .lines
            .push(crate::state::Line::Assistant("gern, ich fange an".into()));
        let content = render_to_text(80, 24, &state);
        assert!(
            content.contains("schreibe eine app"),
            "the user message must render"
        );
        assert!(content.contains("gern, ich fange an"));
        assert!(content.contains("s-1"), "the session id must be visible");
    }

    #[test]
    fn the_approval_modal_shows_the_question() {
        let mut state = TuiState::new("s-1".into(), None, "allow-read-only".into());
        state.pending_approval = Some(crate::state::PendingApproval {
            call_id: "c1".into(),
            name: "execute_command".into(),
            risk: "destructive".into(),
            arguments: "{\"command\": \"rm -rf x\"}".into(),
        });
        let content = render_to_text(80, 24, &state);
        assert!(content.contains("Freigabe"));
        assert!(content.contains("execute_command"));
        assert!(content.contains("erlauben"));
    }

    #[test]
    fn live_deltas_render_before_the_turn_ends() {
        let mut state = TuiState::new("s-1".into(), None, "allow-read-only".into());
        state.live = "hallo wel".into();
        let content = render_to_text(80, 24, &state);
        assert!(
            content.contains("hallo wel"),
            "streamed text must appear immediately"
        );
    }

    #[test]
    fn the_help_overlay_names_the_keys() {
        let mut state = TuiState::new("s-1".into(), None, "allow-read-only".into());
        state.modal = Modal::Help;
        let content = render_to_text(80, 24, &state);
        assert!(content.contains("ctrl+j"));
        assert!(content.contains("/fork"));
    }
}
