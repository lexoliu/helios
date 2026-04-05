use anyhow::{Context as _, Result};
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use ratatui_code_editor::editor::Editor;
use ratatui_code_editor::theme::vesper;
use ratatui_code_editor::utils::get_lang;

use crate::filesystem;
use crate::serial::RpcClient;
use crate::tui;

pub async fn run(client: &mut RpcClient, path: &str) -> Result<()> {
    let path = filesystem::normalize_path(path)?;
    filesystem::touch(client, &path).await?;
    let bytes = filesystem::cat(client, &path).await?;
    let content = String::from_utf8(bytes)
        .with_context(|| format!("edit only supports UTF-8 text files: {path}"))?;
    let mut app = App::new(path, content)?;
    let mut session = tui::Session::open(true, "editor")?;
    let events = tui::spawn_events();

    loop {
        session.terminal().draw(|frame| app.draw(frame))?;
        let Some(event) = events.recv().await.ok() else {
            break;
        };
        if app.handle_event(event, client).await? {
            break;
        }
    }

    session.close()
}

struct App {
    path: String,
    language: String,
    saved_content: String,
    status: String,
    confirm_discard: bool,
    editor: Editor,
    editor_area: Rect,
}

impl App {
    fn new(path: String, content: String) -> Result<Self> {
        let language = get_lang(&path);
        let editor = Editor::new(&language, &content, vesper())
            .with_context(|| format!("failed to create editor for {path}"))?;
        Ok(Self {
            path,
            language,
            saved_content: content,
            status: "Ctrl+S saves. Esc leaves clean buffers. Ctrl+Q discards. Ctrl+C aborts."
                .to_owned(),
            confirm_discard: false,
            editor,
            editor_area: Rect::default(),
        })
    }

    async fn handle_event(
        &mut self,
        event: CrosstermEvent,
        client: &mut RpcClient,
    ) -> Result<bool> {
        match event {
            CrosstermEvent::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                self.handle_key(key, client).await
            }
            CrosstermEvent::Mouse(mouse) => {
                self.editor
                    .mouse(mouse, &self.editor_area)
                    .with_context(|| format!("failed to handle mouse input for {}", self.path))?;
                self.confirm_discard = false;
                Ok(false)
            }
            CrosstermEvent::Resize(_, _) => Ok(false),
            _ => Ok(false),
        }
    }

    async fn handle_key(&mut self, key: KeyEvent, client: &mut RpcClient) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return self.save(client).await.map(|()| false);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return Ok(self.try_exit());
        }

        if key.code == KeyCode::Esc {
            return Ok(self.try_exit());
        }

        self.editor
            .input(key, &self.editor_area)
            .with_context(|| format!("failed to apply editor input for {}", self.path))?;
        self.confirm_discard = false;
        self.status = self.default_status();
        Ok(false)
    }

    async fn save(&mut self, client: &mut RpcClient) -> Result<()> {
        let content = self.editor.get_content();
        filesystem::write(client, &self.path, content.as_bytes(), false)
            .await
            .with_context(|| format!("failed to save {}", self.path))?;
        self.saved_content = content;
        self.confirm_discard = false;
        self.status = format!("saved {}", self.path);
        Ok(())
    }

    fn try_exit(&mut self) -> bool {
        if self.is_dirty() && !self.confirm_discard {
            self.confirm_discard = true;
            self.status = "unsaved changes: Ctrl+S saves, Esc/Ctrl+Q again discards, Ctrl+C aborts"
                .to_owned();
            return false;
        }
        true
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(frame.area());

        frame.render_widget(self.header(), layout[0]);

        let editor_block = Block::default()
            .borders(Borders::ALL)
            .title(" editor ")
            .border_style(Style::default().fg(Color::Rgb(104, 120, 143)));
        self.editor_area = editor_block.inner(layout[1]);
        frame.render_widget(editor_block, layout[1]);
        frame.render_widget(&self.editor, self.editor_area);

        if let Some((x, y)) = self.editor.get_visible_cursor(&self.editor_area) {
            frame.set_cursor_position(Position::new(x, y));
        }

        frame.render_widget(self.footer(), layout[2]);
    }

    fn header(&self) -> Paragraph<'static> {
        Paragraph::new(Line::from(vec![
            Span::styled(
                "edit ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.path.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("lang={}", self.language),
                Style::default().fg(Color::Rgb(165, 252, 182)),
            ),
            Span::raw("  "),
            self.dirty_badge(),
            Span::raw("  "),
            Span::styled(self.cursor_text(), Style::default().fg(Color::DarkGray)),
        ]))
    }

    fn footer(&self) -> Paragraph<'static> {
        Paragraph::new(vec![
            Line::from(vec![Span::styled(
                self.status.clone(),
                Style::default().fg(Color::Rgb(177, 252, 229)),
            )]),
            Line::from(vec![
                Span::styled(
                    "Ctrl+S",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" save  "),
                Span::styled(
                    "Esc",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" close  "),
                Span::styled(
                    "Ctrl+Q",
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" discard  "),
                Span::styled(
                    "Ctrl+C",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" abort"),
            ]),
        ])
    }

    fn dirty_badge(&self) -> Span<'static> {
        if self.is_dirty() {
            Span::styled(
                "modified",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(246, 201, 159))
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                "saved",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(165, 252, 182))
                    .add_modifier(Modifier::BOLD),
            )
        }
    }

    fn is_dirty(&self) -> bool {
        self.editor.get_content() != self.saved_content
    }

    fn cursor_text(&self) -> String {
        let (line, column) = self.editor.code_ref().point(self.editor.get_cursor());
        format!("line {} col {}", line + 1, column + 1)
    }

    fn default_status(&self) -> String {
        if self.is_dirty() {
            "buffer modified; Ctrl+S saves to the remote debugger filesystem".to_owned()
        } else {
            "buffer saved".to_owned()
        }
    }
}
