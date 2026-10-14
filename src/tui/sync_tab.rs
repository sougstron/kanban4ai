//! Global Settings → Sync (TASK-415): this node, the API switch, paired
//! nodes in phone priority order, pairing, sync state and clone status.
//! Every change is a button or a switch and applies at once; Save/Cancel of
//! the dialog never touch it. Store I/O lives in `cli::serve::settings` and
//! `core::net`; this module owns page state, actions and rendering.
use crate::core::net;
use serde_json::Value;
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::cli::serve::settings::{self, View};
use crate::core::sync::NodeRole;

use super::app::{App, HitAction, Hitbox};
use super::card::sanitize_terminal_text;

/// One selectable line of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row {
    Name,
    Role,
    Api,
    NetworkMode,
    AuthKey,
    Connect,
    Disconnect,
    Logout,
    Peer(usize),
    Pair,
    Code,
    Refresh,
}

/// The single-line inputs a button opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormKind {
    Rename,
    Pair,
    Code,
    AuthKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Form {
    pub kind: FormKind,
    pub fields: Vec<(&'static str, String)>,
    pub focus: usize,
}

/// Work the page asks [`App`] to do with the store root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    Load,
    Refresh,
    SaveNode {
        name: String,
        role: NodeRole,
    },
    SetApi(bool),
    MovePeer {
        id: String,
        offset: isize,
    },
    PairingCode {
        address: String,
    },
    Pair {
        address: String,
        code: String,
        local: String,
    },
    Network(NetworkAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NetworkAction {
    Status,
    Mode(net::Mode),
    Connect(String),
    Disconnect,
    Logout,
}
/// Results of work that touches the network and so runs off the UI thread.
pub(crate) enum Outcome {
    View(Result<View, String>),
    Paired(Result<String, String>),
    Network {
        mode: Option<net::Mode>,
        status: Result<Value, String>,
        login: Result<Option<String>, String>,
    },
}

#[derive(Default)]
pub(crate) struct SyncTab {
    pub view: Option<View>,
    pub selected: usize,
    pub form: Option<Form>,
    /// Extra pages below the selected control, for conflicts and clone details.
    pub page: usize,
    /// A fresh pairing code and, for a phone, its QR.
    pub pairing: Option<(String, Option<String>)>,
    /// The last action's result; `true` marks an error.
    pub notice: Option<(String, bool)>,
    pub request: Option<Request>,
    pub job: Option<Receiver<Outcome>>,
    pub network_mode: Option<net::Mode>,
    pub network: Option<Result<Value, String>>,
    pub auth_key: String,
    pub login_url: Option<String>,
    pub login_qr: Option<String>,
    last_network_poll: Option<Instant>,
}

const ROLES: [NodeRole; 3] = [NodeRole::Laptop, NodeRole::Server, NodeRole::Mobile];

fn role_label(role: NodeRole) -> &'static str {
    match role {
        NodeRole::Laptop => "laptop",
        NodeRole::Server => "server",
        NodeRole::Mobile => "mobile",
    }
}

impl SyncTab {
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![
            Row::Name,
            Row::Role,
            Row::Api,
            Row::NetworkMode,
            Row::AuthKey,
            Row::Connect,
            Row::Disconnect,
            Row::Logout,
        ];
        let peers = self.view.as_ref().map_or(0, |view| view.peers.len());
        rows.extend((0..peers).map(Row::Peer));
        rows.extend([Row::Pair, Row::Code, Row::Refresh]);
        rows
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Owns Esc and the arrows while a form or the pairing code is open.
    pub fn overlay_open(&self) -> bool {
        self.form.is_some() || self.pairing.is_some() || self.login_url.is_some()
    }

    pub fn close_overlay(&mut self) {
        self.form = None;
        self.pairing = None;
        self.login_url = None;
    }

    fn open(&mut self, kind: FormKind) {
        let fields = match kind {
            FormKind::Rename => vec![(
                "Name",
                self.view
                    .as_ref()
                    .map(|v| v.name.clone())
                    .unwrap_or_default(),
            )],
            FormKind::Pair => vec![
                ("Peer address (IP:port)", String::new()),
                ("Code shown by the peer", String::new()),
                ("This node's LAN address (IP:port)", String::new()),
            ],
            FormKind::Code => vec![(
                "This node's LAN address for a phone QR (optional)",
                String::new(),
            )],
            FormKind::AuthKey => vec![(
                "Auth key (one-time, never saved)",
                std::mem::take(&mut self.auth_key),
            )],
        };
        self.form = Some(Form {
            kind,
            fields,
            focus: 0,
        });
        self.notice = None;
    }

    pub fn input(&mut self, key: KeyEvent) {
        if self.view.is_none() {
            return;
        }
        if let Some(form) = &mut self.form {
            let last = form.fields.len() - 1;
            match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.fields[form.focus].1.push(c);
                }
                KeyCode::Backspace => {
                    form.fields[form.focus].1.pop();
                }
                KeyCode::Up => form.focus = form.focus.saturating_sub(1),
                KeyCode::Down => form.focus = (form.focus + 1).min(last),
                KeyCode::Enter if form.focus < last => form.focus += 1,
                KeyCode::Enter => self.submit(),
                _ => {}
            }
            return;
        }
        if self.login_url.is_some() {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                self.login_url = None;
            }
            return;
        }
        if self.pairing.is_some() {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                self.pairing = None;
            }
            return;
        }
        let rows = self.rows();
        if matches!(key.code, KeyCode::PageDown | KeyCode::PageUp) {
            self.page = if key.code == KeyCode::PageDown {
                self.page.saturating_add(1)
            } else {
                self.page.saturating_sub(1)
            };
            return;
        }
        self.page = 0;
        let selected = self.selected.min(rows.len() - 1);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match (key.code, rows[selected]) {
            (KeyCode::Up | KeyCode::Down, Row::Peer(index)) if shift => {
                self.move_peer(index, key.code == KeyCode::Up)
            }
            (KeyCode::Char('K'), Row::Peer(index)) => self.move_peer(index, true),
            (KeyCode::Char('J'), Row::Peer(index)) => self.move_peer(index, false),
            (KeyCode::Up, _) => self.selected = selected.saturating_sub(1),
            (KeyCode::Down, _) => self.selected = (selected + 1).min(rows.len() - 1),
            (KeyCode::Home, _) => self.selected = 0,
            (KeyCode::End, _) => self.selected = rows.len() - 1,
            (KeyCode::Char('r'), _) => self.request = Some(Request::Refresh),
            (KeyCode::Enter | KeyCode::Char(' '), row) => self.activate(row),
            _ => {}
        }
    }

    fn move_peer(&mut self, index: usize, up: bool) {
        let Some(view) = &self.view else { return };
        let target = if up {
            index.checked_sub(1)
        } else {
            (index + 1 < view.peers.len()).then_some(index + 1)
        };
        let Some(target) = target else { return };
        self.request = Some(Request::MovePeer {
            id: view.peers[index].id.clone(),
            offset: if up { -1 } else { 1 },
        });
        // The cursor follows the node it moves.
        self.selected = self.selected + target - index;
    }

    fn activate(&mut self, row: Row) {
        let Some(view) = &self.view else { return };
        self.request = match row {
            Row::Name => return self.open(FormKind::Rename),
            Row::Role => {
                let index = ROLES.iter().position(|r| *r == view.role).unwrap_or(0);
                Some(Request::SaveNode {
                    name: view.name.clone(),
                    role: ROLES[(index + 1) % ROLES.len()],
                })
            }
            Row::Api => Some(Request::SetApi(!view.api_enabled)),
            Row::NetworkMode => {
                self.auth_key.clear();
                let next = match self.network_mode.unwrap_or(net::Mode::Off) {
                    net::Mode::Off => net::Mode::System,
                    net::Mode::System => net::Mode::Managed,
                    net::Mode::Managed => net::Mode::Off,
                };
                Some(Request::Network(NetworkAction::Mode(next)))
            }
            Row::AuthKey => return self.open(FormKind::AuthKey),
            Row::Connect | Row::Disconnect | Row::Logout
                if self.network_mode != Some(net::Mode::Managed) =>
            {
                self.notice = Some((
                    "Select Managed; System is read-only (use the system Tailscale app).".into(),
                    true,
                ));
                return;
            }
            Row::Connect => Some(Request::Network(NetworkAction::Connect(std::mem::take(
                &mut self.auth_key,
            )))),
            Row::Disconnect => Some(Request::Network(NetworkAction::Disconnect)),
            Row::Logout => Some(Request::Network(NetworkAction::Logout)),
            Row::Peer(_) => return,
            Row::Pair => return self.open(FormKind::Pair),
            Row::Code => return self.open(FormKind::Code),
            Row::Refresh => Some(Request::Refresh),
        };
    }

    fn submit(&mut self) {
        let Some(form) = self.form.take() else { return };
        let value = |index: usize| form.fields[index].1.trim().to_string();
        if form.kind == FormKind::AuthKey {
            self.auth_key = value(0);
            return;
        }
        self.request = Some(match form.kind {
            FormKind::Rename => Request::SaveNode {
                name: value(0),
                role: self.view.as_ref().map_or(NodeRole::Laptop, |v| v.role),
            },
            FormKind::Pair => Request::Pair {
                address: value(0),
                code: value(1),
                local: value(2),
            },
            FormKind::Code => Request::PairingCode { address: value(0) },
            FormKind::AuthKey => unreachable!(),
        });
    }

    /// Run a request against the store. Network work goes to a thread whose
    /// result [`SyncTab::poll`] collects.
    pub fn execute(&mut self, root: &Path, request: Request) {
        if self.busy() {
            self.request = Some(request);
            return;
        }
        let root = root.to_path_buf();
        let result = match request {
            Request::Network(action) => {
                self.spawn(move || {
                    let login = run_network(&root, action).map_err(|e| e.to_string());
                    Outcome::Network {
                        mode: net::settings(&root).ok().map(|s| s.mode),
                        status: net::status(&root).map_err(|e| e.to_string()),
                        login,
                    }
                });
                return;
            }
            Request::Load => settings::local_view(&root).map(|view| {
                self.view = Some(view);
                self.request = Some(Request::Network(NetworkAction::Status));
                None
            }),
            Request::Refresh => {
                self.spawn(move || {
                    Outcome::View(settings::refresh(&root).map_err(|e| e.to_string()))
                });
                self.notice = Some(("Probing paired nodes…".into(), false));
                return;
            }
            Request::Pair {
                address,
                code,
                local,
            } => {
                let job_root = root.clone();
                self.spawn(move || {
                    Outcome::Paired(
                        settings::pair(&job_root, &address, &code, &local)
                            .map_err(|e| e.to_string()),
                    )
                });
                self.notice = Some(("Pairing…".into(), false));
                return;
            }
            Request::SaveNode { name, role } => settings::save_node(&root, &name, role)
                .map(|()| Some(format!("This node is now {name} ({})", role_label(role)))),
            Request::SetApi(enabled) => settings::set_api(&root, enabled).map(|()| {
                Some(if enabled {
                    "API server enabled and started".to_string()
                } else {
                    "API server stopped and disabled".to_string()
                })
            }),
            Request::MovePeer { id, offset } => {
                settings::move_peer(&root, &id, offset).map(|()| None)
            }
            Request::PairingCode { address } => {
                settings::pairing_code(&root, &address).map(|pairing| {
                    self.pairing = Some(pairing);
                    None
                })
            }
        };
        match result {
            Ok(notice) => {
                if let Some(notice) = notice {
                    self.notice = Some((notice, false));
                }
                self.reload(&root);
            }
            Err(e) => self.notice = Some((e.to_string(), true)),
        }
    }

    fn spawn(&mut self, work: impl FnOnce() -> Outcome + Send + 'static) {
        if self.busy() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        self.job = Some(receiver);
    }

    /// Re-read local state, keeping probe results for nodes still paired.
    fn reload(&mut self, root: &Path) {
        match settings::local_view(root) {
            Ok(mut view) => {
                if let Some(old) = self.view.take().filter(|old| old.probed) {
                    for peer in &mut view.peers {
                        if let Some(probed) = old.peers.iter().find(|p| p.id == peer.id) {
                            peer.code = probed.code.clone();
                            peer.message = probed.message.clone();
                            peer.route = probed.route.clone();
                            peer.clones = probed.clones.clone();
                        }
                    }
                    view.probed = true;
                }
                self.view = Some(view);
            }
            Err(e) => self.notice = Some((e.to_string(), true)),
        }
    }

    /// Collect a finished background job; called from the app tick.
    pub fn poll(&mut self, root: &Path, interval: Duration, active: bool) {
        // Each status runs the tailscale CLI; the page does not need it per tick.
        const NETWORK_STATUS_INTERVAL: Duration = Duration::from_secs(5);
        if active
            && self.view.is_some()
            && !self.busy()
            && self.request.is_none()
            && self
                .last_network_poll
                .is_none_or(|at| at.elapsed() >= interval.max(NETWORK_STATUS_INTERVAL))
        {
            self.request = Some(Request::Network(NetworkAction::Status));
        }
        let Some(job) = &self.job else { return };
        let outcome = match job.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                self.notice = Some(("Background sync job stopped".into(), true));
                return;
            }
        };
        self.job = None;
        match outcome {
            Outcome::Network {
                mode,
                status,
                login,
            } => {
                self.network_mode = mode;
                if status
                    .as_ref()
                    .is_ok_and(|v| v["BackendState"] == "Running")
                {
                    self.login_url = None;
                }
                self.network = Some(status);
                self.last_network_poll = Some(Instant::now());
                match login {
                    Ok(Some(url)) => {
                        self.login_qr = qrcode::QrCode::new(url.as_bytes())
                            .ok()
                            .map(|qr| qr.render::<qrcode::render::unicode::Dense1x2>().build());
                        self.login_url = Some(url);
                    }
                    Ok(None) => {}
                    Err(error) => self.notice = Some((error, true)),
                }
            }
            Outcome::View(Ok(view)) => {
                self.view = Some(view);
                self.notice = None;
            }
            Outcome::View(Err(e)) => self.notice = Some((e, true)),
            Outcome::Paired(Ok(name)) => {
                self.notice = Some((format!("Paired with {name}"), false));
                self.reload(root);
            }
            Outcome::Paired(Err(e)) => self.notice = Some((e, true)),
        }
    }
}

fn run_network(root: &Path, action: NetworkAction) -> crate::core::error::Result<Option<String>> {
    match action {
        NetworkAction::Status => {}
        NetworkAction::Connect(key) => return net::connect(root, &key),
        NetworkAction::Disconnect => net::down(root)?,
        NetworkAction::Logout => net::logout(root)?,
        NetworkAction::Mode(mode) => settings::set_network_mode(root, mode)?,
    }
    Ok(None)
}

fn render_login(frame: &mut Frame<'_>, app: &App, url: &str, qr: Option<&str>) {
    let area = Rect {
        height: frame.area().height.saturating_sub(1),
        ..frame.area()
    };
    let block = Block::default()
        .title(" Tailscale login ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.focus));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from("Open this link or scan the QR:"),
        Line::from(sanitize_terminal_text(url)),
    ];
    let link_height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner.width);
    if let Some(qr) = qr {
        let width = qr.lines().map(|s| s.chars().count()).max().unwrap_or(0);
        let height = qr.lines().count();
        if width <= usize::from(inner.width)
            && link_height + height + 2 <= usize::from(inner.height)
        {
            lines.extend(qr.lines().map(|s| Line::from(s.to_owned())));
        } else {
            lines.push(Line::from(
                "QR needs a wider/taller terminal; use the link.",
            ));
        }
    }
    lines.push(Line::from("Waiting for login… Enter/Esc closes."));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
fn heading(app: &App, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(app.theme.fg)
            .add_modifier(Modifier::BOLD),
    ))
}

fn detail(app: &App, text: String) -> Line<'static> {
    Line::from(Span::styled(
        format!("      {}", sanitize_terminal_text(&text)),
        Style::default().fg(app.theme.muted),
    ))
}

fn status_style(app: &App, code: &str) -> Style {
    Style::default().fg(match code {
        "ONLINE" => app.theme.ok,
        "" => app.theme.muted,
        "NODE_OUT_OF_LAN" | "NODE_POWERED_OFF" => app.theme.warn,
        _ => app.theme.err,
    })
}

/// The page as lines plus the line index of every selectable row.
fn lines(
    app: &App,
    tab: &SyncTab,
    view: &View,
    active: bool,
) -> (Vec<Line<'static>>, usize, Vec<(usize, usize)>) {
    let rows = tab.rows();
    let selected = rows[tab.selected.min(rows.len() - 1)];
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut controls = Vec::new();
    let mut row = |out: &mut Vec<Line<'static>>, which: Row, spans: Vec<Span<'static>>| {
        let current = which == selected;
        if current {
            cursor = out.len();
        }
        let marker = if current && active { "› " } else { "  " };
        let mut line = vec![Span::styled(marker, Style::default().fg(app.theme.focus))];
        line.extend(spans.into_iter().map(|span| {
            if current && active {
                span.patch_style(
                    Style::default()
                        .fg(app.theme.focus)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                span
            }
        }));
        controls.push((
            rows.iter().position(|row| *row == which).unwrap(),
            out.len(),
        ));
        out.push(Line::from(line));
    };
    let muted = Style::default().fg(app.theme.muted);
    out.push(heading(app, "This node"));
    row(
        &mut out,
        Row::Name,
        vec![
            Span::raw(format!("Name: {}", sanitize_terminal_text(&view.name))),
            Span::styled("  (Enter renames)", muted),
        ],
    );
    row(
        &mut out,
        Row::Role,
        vec![
            Span::raw(format!("Role: ◀ {} ▶", role_label(view.role))),
            Span::styled("  (Space changes)", muted),
        ],
    );
    let mark = if view.api_enabled { "☑" } else { "☐" };
    let state = match (view.role, view.api_active) {
        (NodeRole::Mobile, _) => "unused on a phone",
        (_, Some(true)) => "running",
        (_, Some(false)) => "not running",
        (_, None) => "state unknown",
    };
    row(
        &mut out,
        Row::Api,
        vec![
            Span::raw(format!("{mark} API server enabled (user unit)")),
            Span::styled(format!(" · {state}"), muted),
        ],
    );
    out.push(Line::default());
    out.push(heading(app, "Сеть / Network"));
    let mode = match tab.network_mode.unwrap_or(net::Mode::Off) {
        net::Mode::Off => "Off",
        net::Mode::System => "System",
        net::Mode::Managed => "Managed",
    };
    row(
        &mut out,
        Row::NetworkMode,
        vec![Span::raw(format!("Mode: ◀ {mode} ▶"))],
    );
    match &tab.network {
        Some(Ok(status)) => {
            out.push(detail(
                app,
                format!(
                    "Status: {}",
                    status["BackendState"].as_str().unwrap_or("unknown")
                ),
            ));
            let user_id = status["Self"]["UserID"]
                .as_u64()
                .map(|id| id.to_string())
                .unwrap_or_default();
            let account = status["User"][&user_id]["LoginName"]
                .as_str()
                .unwrap_or("—");
            out.push(detail(app, format!("Account: {account}")));
            let ips = status["TailscaleIPs"]
                .as_array()
                .map(|ips| {
                    ips.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "—".into());
            out.push(detail(app, format!("Tailnet IP: {ips}")));
            let peers = status["Peer"].as_object();
            let online = peers.map_or(0, |peers| {
                peers.values().filter(|peer| peer["Online"] == true).count()
            });
            out.push(detail(app, format!("Peers online: {online}")));
            if let Some(peers) = peers {
                for peer in peers.values().filter(|peer| peer["Online"] == true) {
                    let route = if peer["CurAddr"].as_str().is_some_and(|s| !s.is_empty()) {
                        "direct".to_string()
                    } else if let Some(relay) = peer["Relay"].as_str().filter(|s| !s.is_empty()) {
                        format!("DERP ({relay})")
                    } else {
                        "path unknown".into()
                    };
                    out.push(detail(
                        app,
                        format!("{}: {route}", peer["HostName"].as_str().unwrap_or("peer")),
                    ));
                }
            }
        }
        Some(Err(error)) => out.push(detail(app, error.clone())),
        None => out.push(detail(app, "Status: not refreshed".into())),
    }
    if tab.network_mode == Some(net::Mode::System) {
        out.push(detail(
            app,
            "System is read-only; use the installed Tailscale app.".into(),
        ));
    }
    row(
        &mut out,
        Row::AuthKey,
        vec![Span::raw(format!(
            "Auth key: {} (one-time)",
            if tab.auth_key.is_empty() {
                "empty"
            } else {
                "••••••••"
            }
        ))],
    );
    row(
        &mut out,
        Row::Connect,
        vec![Span::raw("[ Подключить / Connect ]")],
    );
    row(
        &mut out,
        Row::Disconnect,
        vec![Span::raw("[ Отключить / Disconnect ]")],
    );
    row(
        &mut out,
        Row::Logout,
        vec![Span::raw("[ Выйти из аккаунта / Logout ]")],
    );
    out.push(Line::default());
    out.push(heading(
        app,
        "Nodes in phone priority order (Shift+↑/↓ or K/J moves)",
    ));
    if view.peers.is_empty() {
        out.push(Line::from(Span::styled("  No paired nodes yet", muted)));
    }
    for (index, peer) in view.peers.iter().enumerate() {
        let mut spans = vec![Span::raw(format!(
            "{}. {} ({})",
            index + 1,
            sanitize_terminal_text(&peer.name),
            role_label(peer.role)
        ))];
        if !peer.code.is_empty() {
            spans.push(Span::styled(
                format!(" · {}", peer.code),
                status_style(app, &peer.code),
            ));
        }
        row(&mut out, Row::Peer(index), spans);
        out.push(Line::from(Span::styled(
            format!("      {}", sanitize_terminal_text(&peer.message)),
            status_style(app, &peer.code),
        )));
        out.push(detail(
            app,
            format!(
                "{} · {} · {} op(s) waiting",
                peer.route,
                peer.exchange
                    .as_deref()
                    .map_or("never exchanged".to_string(), |at| format!(
                        "exchanged {at}"
                    )),
                peer.pending
            ),
        ));
        for clone in &peer.clones {
            out.push(detail(app, format!("clone {clone}")));
        }
    }
    row(
        &mut out,
        Row::Pair,
        vec![Span::raw("[ Pair with a node… ]")],
    );
    row(
        &mut out,
        Row::Code,
        vec![Span::raw("[ Show a pairing code / phone QR… ]")],
    );
    let refresh = if tab.busy() {
        "[ Refreshing… ]"
    } else {
        "[ Refresh status ]"
    };
    row(
        &mut out,
        Row::Refresh,
        vec![Span::raw(refresh), Span::styled("  (r)", muted)],
    );
    out.push(Line::default());
    out.push(heading(
        app,
        &format!("Conflicts ({})", view.conflicts.len()),
    ));
    if view.conflicts.is_empty() {
        out.push(Line::from(Span::styled("  None", muted)));
    }
    for conflict in &view.conflicts {
        out.push(Line::from(format!(
            "  {}",
            sanitize_terminal_text(conflict)
        )));
    }
    out.push(Line::default());
    out.push(heading(app, "Project clones on this node"));
    if view.local_clones.is_empty() {
        out.push(Line::from(Span::styled("  No registered projects", muted)));
    }
    for clone in &view.local_clones {
        out.push(Line::from(format!("  {}", sanitize_terminal_text(clone))));
    }
    (out, cursor, controls)
}

pub(crate) fn render(
    frame: &mut Frame<'_>,
    app: &App,
    tab: &SyncTab,
    active: bool,
    inside: bool,
    area: Rect,
    hitboxes: &mut Vec<Hitbox>,
) {
    let border = if active {
        app.theme.focus
    } else {
        app.theme.border
    };
    // The row cursor shows only once Enter stepped inside the panel.
    let mut block = Block::default()
        .title(" Sync ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    if inside {
        block = block.border_type(ratatui::widgets::BorderType::Thick);
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(view) = &tab.view else {
        let text = match &tab.notice {
            Some((text, true)) => text.clone(),
            _ => "Loading…".to_string(),
        };
        frame.render_widget(
            Paragraph::new(sanitize_terminal_text(&text))
                .style(Style::default().fg(app.theme.muted)),
            inner,
        );
        return;
    };
    let (text, cursor, controls) = lines(app, tab, view, inside);
    let notice_height = u16::from(tab.notice.is_some() && inner.height > 2);
    let body = Rect {
        height: inner.height.saturating_sub(notice_height),
        ..inner
    };
    // Match ratatui's actual word wrapping, including wide Unicode.
    let wrapped = |lines: &[Line<'static>]| {
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(body.width)
    };
    let visible = usize::from(body.height);
    let total = wrapped(&text);
    let cursor = wrapped(&text[..cursor]);
    let scroll = (cursor + 2)
        .saturating_sub(visible)
        .saturating_add(tab.page.saturating_mul(visible.saturating_sub(1).max(1)))
        .min(total.saturating_sub(visible));
    if !tab.overlay_open() {
        for (index, line) in controls {
            let start = wrapped(&text[..line]);
            let end = start + wrapped(&text[line..=line]);
            let top = start.max(scroll);
            let bottom = end.min(scroll.saturating_add(visible));
            if top < bottom {
                hitboxes.push(Hitbox {
                    area: Rect {
                        y: body.y + (top - scroll) as u16,
                        height: (bottom - top) as u16,
                        ..body
                    },
                    action: HitAction::ModalOption {
                        field: super::dialogs::DialogField::SyncPanel,
                        index,
                    },
                });
            }
        }
    }
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        body,
    );
    if let (Some((notice, error)), 1) = (&tab.notice, notice_height) {
        frame.render_widget(
            Paragraph::new(sanitize_terminal_text(notice)).style(Style::default().fg(if *error {
                app.theme.err
            } else {
                app.theme.ok
            })),
            Rect {
                y: inner.y + inner.height - 1,
                height: 1,
                ..inner
            },
        );
    }
    if let Some(url) = &tab.login_url {
        render_login(frame, app, url, tab.login_qr.as_deref());
    }
    if let Some(form) = &tab.form {
        render_form(frame, app, form, inner);
    } else if let Some(pairing) = &tab.pairing {
        let area = if pairing.1.is_some() {
            frame.area()
        } else {
            inner
        };
        render_pairing(frame, app, pairing, area);
    }
}

fn overlay(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    Rect {
        y: area.y + (area.height - height) / 2,
        height,
        ..area
    }
}

fn render_form(frame: &mut Frame<'_>, app: &App, form: &Form, area: Rect) {
    let title = match form.kind {
        FormKind::Rename => " Rename this node ",
        FormKind::Pair => " Pair with a node ",
        FormKind::Code => " Pairing code ",
        FormKind::AuthKey => " One-time Tailscale auth key ",
    };
    let mut lines = Vec::new();
    for (index, (label, value)) in form.fields.iter().enumerate() {
        let focused = index == form.focus;
        lines.push(Line::from(Span::styled(
            label.to_string(),
            Style::default().fg(app.theme.muted),
        )));
        let caret = if focused { "▏" } else { "" };
        lines.push(Line::from(Span::styled(
            format!(
                "{}{caret}",
                if form.kind == FormKind::AuthKey {
                    "•".repeat(
                        value
                            .chars()
                            .count()
                            .min(usize::from(area.width.saturating_sub(3))),
                    )
                } else {
                    sanitize_terminal_text(value)
                }
            ),
            if focused {
                Style::default().fg(app.theme.focus)
            } else {
                Style::default().fg(app.theme.fg)
            },
        )));
    }
    lines.push(Line::from(Span::styled(
        "Enter: next / confirm · ↑/↓: field · Esc: cancel",
        Style::default().fg(app.theme.muted),
    )));
    let height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width.saturating_sub(2));
    let area = overlay(area, u16::try_from(height + 2).unwrap_or(u16::MAX));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.focus)),
        ),
        area,
    );
}

fn render_pairing(
    frame: &mut Frame<'_>,
    app: &App,
    pairing: &(String, Option<String>),
    area: Rect,
) {
    let (code, qr) = pairing;
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Code: "),
            Span::styled(
                code.clone(),
                Style::default()
                    .fg(app.theme.focus)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "Enter it on the other node: Sync → Pair with a node.",
            Style::default().fg(app.theme.muted),
        )),
    ];
    if let Some(qr) = qr {
        lines.push(Line::from("Scan with the phone:"));
        lines.extend(qr.lines().map(|line| Line::from(line.to_string())));
    }
    lines.push(Line::from(Span::styled(
        "Enter or Esc closes",
        Style::default().fg(app.theme.muted),
    )));
    let area = overlay(area, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Pairing code ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.focus)),
        ),
        area,
    );
}
