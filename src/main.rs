use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use base64::Engine;
use futures::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Entry, Label, ListBox, ListBoxRow,
    Orientation, ScrolledWindow, SelectionMode, TextBuffer, TextView, WrapMode,
};
use irc::client::prelude::*;
use irc::client::ClientStream;
use irc::proto::{CapSubCommand, Response};

mod config;
use config::ServerConfig;

const APP_ID: &str = "org.example.squawkline";
const SERVER_TARGET: &str = "(server)";
// Separates the server name from the target in composite keys; chosen
// because it can't appear in an IRC server name or channel/nick name.
const KEY_SEP: char = '\u{2}';

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

// Extracts and formats the "time" message tag (IRCv3 server-time cap):
// an RFC3339 UTC timestamp the server attaches to a message, distinct from
// whenever we happened to receive it.
fn tag_time(message: &Message) -> Option<String> {
    let raw = message.tags.as_ref()?.iter().find(|t| t.0 == "time")?.1.as_deref()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    Some(parsed.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

// user@host from the message's own prefix, when present — used for the
// WeeChat-style JOIN line (nick [account] (realname) (user@host) has
// joined #channel).
fn userhost(message: &Message) -> Option<String> {
    match &message.prefix {
        Some(Prefix::Nickname(_, user, host)) if !user.is_empty() && !host.is_empty() => Some(format!("{user}@{host}")),
        _ => None,
    }
}

// Builds the reply body (without \x01 delimiters) for a known CTCP request,
// following the same set WeeChat answers by default (irc-ctcp.c): PING
// echoes the requester's payload back, the rest are static/derived info.
fn ctcp_reply(ctcp: &str) -> Option<String> {
    let mut parts = ctcp.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("").to_uppercase();
    let args = parts.next().unwrap_or("");

    match cmd.as_str() {
        "PING" => Some(format!("PING {args}").trim_end().to_owned()),
        "VERSION" => Some(format!("VERSION squawkline {}", env!("CARGO_PKG_VERSION"))),
        "TIME" => Some(format!("TIME {}", chrono::Local::now().to_rfc2822())),
        "CLIENTINFO" => Some("CLIENTINFO PING VERSION TIME CLIENTINFO SOURCE".to_owned()),
        "SOURCE" => Some("SOURCE https://github.com/example/squawkline".to_owned()),
        _ => None,
    }
}

// `Message::to_string()` reconstructs the raw wire line, tags included —
// fine for logging, but with server-time (or any tag-granting cap) enabled
// every fallback-displayed line would otherwise show a leading
// "@time=... " prefix the user never asked to see.
fn display_raw(message: &Message) -> String {
    let raw = message.to_string();
    let raw = raw.trim_end();
    match raw.strip_prefix('@').and_then(|s| s.split_once(' ')) {
        Some((_, rest)) => rest.to_owned(),
        None => raw.to_owned(),
    }
}

fn key(server: &str, target: &str) -> String {
    format!("{server}{KEY_SEP}{target}")
}

fn split_key(key: &str) -> (String, String) {
    match key.split_once(KEY_SEP) {
        Some((server, target)) => (server.to_owned(), target.to_owned()),
        None => (key.to_owned(), String::new()),
    }
}

// What the GTK main loop receives from an IRC background thread.
enum IrcEvent {
    // A line to append to the `server`/`target` buffer. `time`, when set,
    // came from the server's own "time" message tag (server-time cap) —
    // used instead of local receive time so replayed/delayed messages show
    // when they actually happened, not when we happened to see them.
    Line { server: String, target: String, text: String, time: Option<String> },
    // The full, current member list for `server`/`channel` (replaces, not merges).
    Names { server: String, channel: String, nicks: Vec<NickInfo> },
}

// A nicklist entry enriched with away status from WHOX, when known.
// Account is deliberately not shown here — matching WeeChat's nicklist
// (irc-nick.c tracks account per-nick but never renders it there); account
// info instead surfaces contextually via extended-join and account-notify
// text lines, which we already show.
#[derive(Clone)]
struct NickInfo {
    nick: String,
    away: bool,
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let sidebar = ListBox::builder()
        .selection_mode(SelectionMode::Single)
        .width_request(180)
        .build();

    let text_view = TextView::builder().editable(false).wrap_mode(WrapMode::WordChar).build();
    let scroller = ScrolledWindow::builder()
        .child(&text_view)
        .vexpand(true)
        .build();

    let entry = Entry::builder()
        .placeholder_text("Message or /command…")
        .build();

    let chat_side = GtkBox::new(Orientation::Vertical, 4);
    chat_side.append(&scroller);
    chat_side.append(&entry);
    chat_side.set_hexpand(true);

    let nicklist = ListBox::builder().selection_mode(SelectionMode::None).build();
    // width_request goes on the scroller, not the list: a ScrolledWindow
    // has its own (small) default minimum size and does not inherit its
    // child's requested size, so setting it on `nicklist` has no effect.
    let nicklist_scroller = ScrolledWindow::builder()
        .child(&nicklist)
        .vexpand(true)
        .hexpand(false)
        .width_request(140)
        .build();

    let layout = GtkBox::new(Orientation::Horizontal, 4);
    layout.append(&sidebar);
    layout.append(&chat_side);
    layout.append(&nicklist_scroller);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Squawkline")
        .default_width(1000)
        .default_height(500)
        .child(&layout)
        .build();
    window.present();

    let cfg = config::load_or_init();

    let buffers: Rc<RefCell<HashMap<String, TextBuffer>>> = Rc::new(RefCell::new(HashMap::new()));
    let rows: Rc<RefCell<HashMap<String, ListBoxRow>>> = Rc::new(RefCell::new(HashMap::new()));
    // Ordered list of composite keys per server, so new rows can be
    // inserted right after their server's existing rows instead of always
    // landing at the bottom of the (otherwise unsorted) sidebar.
    let server_rows: Rc<RefCell<HashMap<String, Vec<String>>>> = Rc::new(RefCell::new(HashMap::new()));
    let server_order: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let channel_nicks: Rc<RefCell<HashMap<String, Vec<NickInfo>>>> = Rc::new(RefCell::new(HashMap::new()));
    let selected: Rc<RefCell<(String, String)>> = Rc::new(RefCell::new((String::new(), String::new())));

    let get_or_create_buffer = {
        let buffers = buffers.clone();
        let rows = rows.clone();
        let server_rows = server_rows.clone();
        let server_order = server_order.clone();
        let sidebar = sidebar.clone();
        move |server: &str, target: &str| -> TextBuffer {
            let k = key(server, target);
            if let Some(buf) = buffers.borrow().get(&k) {
                return buf.clone();
            }
            let buf = TextBuffer::builder().build();
            buffers.borrow_mut().insert(k.clone(), buf.clone());

            let label_text = if target == SERVER_TARGET {
                format!("▾ {server}")
            } else {
                format!("    {target}")
            };
            let row = ListBoxRow::new();
            row.set_widget_name(&k);
            row.set_child(Some(&Label::builder().label(&label_text).xalign(0.0).build()));

            // Insert right after this server's other rows (headers first,
            // channels below in join order) instead of at the sidebar's end.
            let mut order = server_order.borrow_mut();
            if !order.contains(&server.to_owned()) {
                order.push(server.to_owned());
            }
            let mut srows = server_rows.borrow_mut();
            let already_there = srows.get(server).map(Vec::len).unwrap_or(0);
            let position: i32 = order
                .iter()
                .take_while(|s| s.as_str() != server)
                .map(|s| srows.get(s).map(Vec::len).unwrap_or(0))
                .sum::<usize>() as i32
                + already_there as i32;
            srows.entry(server.to_owned()).or_default().push(k.clone());
            drop(srows);

            sidebar.insert(&row, position);
            rows.borrow_mut().insert(k.clone(), row);

            buf
        }
    };

    // Clears and repopulates the nicklist widget from `channel_nicks` for
    // whichever (server, target) is passed in; no-op for non-channels.
    let refresh_nicklist = {
        let channel_nicks = channel_nicks.clone();
        let nicklist = nicklist.clone();
        move |server: &str, target: &str| {
            while let Some(row) = nicklist.row_at_index(0) {
                nicklist.remove(&row);
            }
            if let Some(nicks) = channel_nicks.borrow().get(&key(server, target)) {
                for info in nicks {
                    let label = Label::builder().label(&info.nick).xalign(0.0).build();
                    if info.away {
                        // Dim rather than hide — still relevant to know
                        // who's around, just not actively present.
                        label.set_opacity(0.5);
                    }
                    let row = ListBoxRow::new();
                    row.set_child(Some(&label));
                    nicklist.append(&row);
                }
            }
        }
    };

    // Pre-create a "(server)" header row for every configured server, in
    // config order, before any connection has even started.
    for server_cfg in &cfg.servers {
        get_or_create_buffer(&server_cfg.name, SERVER_TARGET);
    }
    if let Some(first) = cfg.servers.first() {
        let k = key(&first.name, SERVER_TARGET);
        text_view.set_buffer(buffers.borrow().get(&k));
        if let Some(row) = rows.borrow().get(&k) {
            sidebar.select_row(Some(row));
        }
        *selected.borrow_mut() = (first.name.clone(), SERVER_TARGET.to_owned());
    }

    sidebar.connect_row_selected({
        let buffers = buffers.clone();
        let text_view = text_view.clone();
        let selected = selected.clone();
        let refresh_nicklist = refresh_nicklist.clone();
        move |_, row| {
            let Some(row) = row else { return };
            let k = row.widget_name().to_string();
            if let Some(buf) = buffers.borrow().get(&k) {
                text_view.set_buffer(Some(buf));
            }
            let (server, target) = split_key(&k);
            refresh_nicklist(&server, &target);
            *selected.borrow_mut() = (server, target);
        }
    });

    let (irc_tx, irc_rx) = async_channel::unbounded::<IrcEvent>();
    // One outgoing channel per server, keyed by server name, so the input
    // box can route to whichever connection the selected row belongs to.
    let out_senders: Rc<RefCell<HashMap<String, async_channel::Sender<String>>>> = Rc::new(RefCell::new(HashMap::new()));

    // Each configured server gets its own OS thread + tokio runtime + IRC
    // connection, so one network hiccup can't stall the others.
    for server_cfg in cfg.servers {
        let (out_tx, out_rx) = async_channel::unbounded::<String>();
        out_senders.borrow_mut().insert(server_cfg.name.clone(), out_tx);
        let irc_tx = irc_tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
            rt.block_on(run_irc(server_cfg, irc_tx, out_rx));
        });
    }

    // User hits Enter -> forward raw text plus the currently selected
    // (server, target) so the right connection sends to the right place.
    entry.connect_activate({
        let selected = selected.clone();
        let out_senders = out_senders.clone();
        move |entry| {
            let text = entry.text().to_string();
            if text.is_empty() {
                return;
            }
            let (server, target) = selected.borrow().clone();
            if let Some(out_tx) = out_senders.borrow().get(&server) {
                let _ = out_tx.send_blocking(format!("{target}\u{1}{text}"));
                entry.set_text("");
            }
        }
    });

    // Pull routed lines off the channel on the GTK main context, creating
    // buffers/rows on demand — this closure runs on the UI thread only.
    glib::spawn_future_local(async move {
        while let Ok(event) = irc_rx.recv().await {
            match event {
                IrcEvent::Line { server, target, text, time } => {
                    let buf = get_or_create_buffer(&server, &target);
                    let mut end = buf.end_iter();
                    buf.insert(&mut end, &format!("[{}] {}\n", time.unwrap_or_else(timestamp), text));

                    // Only auto-scroll if the (server, target) that just got
                    // a line is the one currently visible.
                    if *selected.borrow() == (server, target) {
                        let mut end = buf.end_iter();
                        text_view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
                    }
                }
                IrcEvent::Names { server, channel, nicks } => {
                    channel_nicks.borrow_mut().insert(key(&server, &channel), nicks);
                    if *selected.borrow() == (server.clone(), channel.clone()) {
                        refresh_nicklist(&server, &channel);
                    }
                }
            }
        }
    });
}

async fn run_irc(cfg: ServerConfig, tx: async_channel::Sender<IrcEvent>, out_rx: async_channel::Receiver<String>) {
    let server_name = cfg.name.clone();
    let send = |target: &str, text: String| {
        let tx = tx.clone();
        let server = server_name.clone();
        let target = target.to_owned();
        async move {
            let _ = tx.send(IrcEvent::Line { server, target, text, time: None }).await;
        }
    };

    let sasl_account = cfg.sasl_account.clone().or_else(|| Some(cfg.nickname.clone()));
    let sasl_password = cfg.sasl_password.clone();

    let irc_config = Config {
        nickname: Some(cfg.nickname),
        server: Some(cfg.server),
        port: cfg.port,
        use_tls: Some(cfg.use_tls),
        channels: cfg.channels,
        ..Config::default()
    };

    let mut client = match Client::from_config(irc_config).await {
        Ok(c) => c,
        Err(e) => {
            send(SERVER_TARGET, format!("connect error: {e}")).await;
            return;
        }
    };

    let mut stream = match client.stream() {
        Ok(s) => s,
        Err(e) => {
            send(SERVER_TARGET, format!("stream error: {e}")).await;
            return;
        }
    };

    // Negotiate IRCv3 capabilities (and SASL, if configured) *before*
    // completing registration — modeled on WeeChat's default of requesting
    // every capability the server offers. identify() below sends the
    // CAP END that actually closes out negotiation and proceeds to
    // NICK/USER, so it must run after this, not instead of this.
    let mut granted_caps = negotiate(&client, &server_name, sasl_account.as_deref(), sasl_password.as_deref(), &mut stream, &tx).await;
    if !granted_caps.is_empty() {
        let mut caps: Vec<&str> = granted_caps.iter().map(String::as_str).collect();
        caps.sort_unstable();
        send(SERVER_TARGET, format!("capabilities enabled: {}", caps.join(", "))).await;
    }

    if let Err(e) = client.identify() {
        send(SERVER_TARGET, format!("identify error: {e}")).await;
        return;
    }

    // Members-per-channel, kept in sync via NAMES replies and JOIN/PART/
    // QUIT/KICK/NICK so the UI's nicklist can just mirror this on change.
    let mut channel_members: HashMap<String, Vec<String>> = HashMap::new();
    // Accumulates RPL_NAMREPLY (353) lines until RPL_ENDOFNAMES (366).
    let mut names_buffer: HashMap<String, Vec<String>> = HashMap::new();
    // Batches in progress (batch cap): reference id -> (batch type, buffered
    // child messages), populated by BATCH +id and flushed on BATCH -id.
    let mut active_batches: HashMap<String, (Option<String>, Vec<Message>)> = HashMap::new();
    // CHANTYPES/PREFIX/WHOX from RPL_ISUPPORT (005), updated as it arrives.
    let mut features = ServerFeatures::default();
    // Away status per nick (WHOX, kept fresh by away-notify) — a user
    // property, not a per-channel one, so it's keyed by nick alone and
    // shared across every channel on this server.
    let mut nick_status: HashMap<String, bool> = HashMap::new();

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming.transpose().unwrap_or(None) {
                    Some(message) => {
                        route_incoming(
                            &server_name, &client, &message, &tx,
                            &mut channel_members, &mut names_buffer,
                            &mut granted_caps, &mut active_batches, &mut features,
                            &mut nick_status,
                        ).await;
                    }
                    None => break,
                }
            }
            outgoing = out_rx.recv() => {
                let Ok(line) = outgoing else { break };
                // Entry side packs "<selected-target>\x01<text>" so we know
                // which buffer a plain (non-/command) message targets.
                let mut split = line.splitn(2, '\u{1}');
                let default_target = split.next().unwrap_or(SERVER_TARGET).to_owned();
                let text = split.next().unwrap_or("");
                if let Err(e) = handle_outgoing(&server_name, &client, &default_target, text, &tx, &granted_caps).await {
                    send(SERVER_TARGET, format!("error: {e}")).await;
                }
            }
        }
    }
}

// The capabilities we ask for if the server offers them — the same set
// WeeChat enables by default (minus a few we have no use for yet, like
// metadata/monitor). "sasl" is added separately, only when a password is
// configured, since requesting it with nothing to authenticate is pointless.
// Also used post-registration (cap-notify) to decide which newly-offered
// capabilities are worth auto-requesting via CAP NEW.
const WANTED_CAPS: [Capability; 14] = [
    Capability::MultiPrefix,
    Capability::AwayNotify,
    Capability::AccountNotify,
    Capability::ChgHost,
    Capability::EchoMessage,
    Capability::ServerTime,
    Capability::CapNotify,
    Capability::InviteNotify,
    Capability::UserhostInNames,
    Capability::Batch,
    Capability::ExtendedJoin,
    Capability::Custom("message-tags"),
    Capability::Custom("draft/chathistory"),
    Capability::Custom("draft/multiline"),
];

// RPL_ISUPPORT (005) tokens we actually act on: CHANTYPES tells us which
// prefix characters mean "this target is a channel" (used instead of a
// hardcoded guess), PREFIX's symbol half tells us which characters in a
// NAMES reply are rank markers to strip rather than part of the nick.
// Defaults are the common case, in effect until/unless a server says
// otherwise or for a server that never sends ISUPPORT at all.
struct ServerFeatures {
    chantypes: String,
    prefixes: String,
    // WHOX (bare "WHOX" ISUPPORT token, no value): lets WHO be asked for
    // exactly the fields we want (nick/away-flags/account) instead of the
    // fixed legacy WHOREPLY shape, and is supported by every real-world
    // ircd checked (InspIRCd, UnrealIRCd, Solanum, Ergo).
    whox: bool,
}

impl Default for ServerFeatures {
    fn default() -> Self {
        Self { chantypes: "#&".to_owned(), prefixes: "@+".to_owned(), whox: false }
    }
}

impl ServerFeatures {
    fn apply_isupport(&mut self, tokens: &[String]) {
        for tok in tokens {
            if let Some(v) = tok.strip_prefix("CHANTYPES=") {
                self.chantypes = v.to_owned();
            } else if let Some(v) = tok.strip_prefix("PREFIX=") {
                // Format is "(modes)symbols", e.g. "(ov)@+" — modes and
                // symbols are positionally paired; we only need the symbols.
                if let Some(close) = v.find(')') {
                    self.prefixes = v[close + 1..].to_owned();
                }
            } else if tok.split('=').next().unwrap_or(tok).eq_ignore_ascii_case("WHOX") {
                self.whox = true;
            }
        }
    }
}

// The capability list lands in the 3rd tuple field of `Command::CAP` for
// the common 3-arg wire shape ("<nick> SUB :caps") and only in the 4th for
// the rarer 4-arg shape (used by LS continuations). Shared by every CAP
// sub-command handler so this doesn't get re-derived (and re-broken) per
// call site — reading only the 4th field once cost us a live duplicate-
// message bug (see git history) because it's `None` in the common case.
fn cap_arg<'a>(third: &'a Option<String>, fourth: &'a Option<String>) -> &'a str {
    fourth.as_deref().or(third.as_deref()).unwrap_or("")
}

/// Runs CAP LS -> CAP REQ -> (optionally) SASL PLAIN, entirely before
/// registration. Does *not* send CAP END — the caller finishes with
/// `client.identify()`, which sends CAP END/NICK/USER. Any message that
/// doesn't belong to the negotiation itself (e.g. "*** Looking up your
/// hostname" NOTICEs some servers send at this stage) is still forwarded
/// to (server) rather than silently dropped.
async fn negotiate(
    client: &Client,
    server_name: &str,
    sasl_account: Option<&str>,
    sasl_password: Option<&str>,
    stream: &mut ClientStream,
    tx: &async_channel::Sender<IrcEvent>,
) -> HashSet<String> {
    let log = |text: String| {
        let tx = tx.clone();
        let server = server_name.to_owned();
        async move {
            let _ = tx.send(IrcEvent::Line { server, target: SERVER_TARGET.to_owned(), text, time: None }).await;
        }
    };

    if client.send_cap_ls(NegotiationVersion::V302).is_err() {
        return HashSet::new();
    }

    // Accumulate CAP LS (possibly multi-line, continued via a "*" marker
    // under IRCv3.2) until the server signals it's done listing.
    let mut offered: HashSet<String> = HashSet::new();
    loop {
        let Some(Ok(message)) = stream.next().await else { return HashSet::new() };
        match &message.command {
            Command::CAP(_, CapSubCommand::LS, third, fourth) => {
                let (more, text) = match (third.as_deref(), fourth.as_deref()) {
                    (Some("*"), Some(list)) => (true, list),
                    (Some(list), None) => (false, list),
                    _ => (false, ""),
                };
                for tok in text.split_whitespace() {
                    offered.insert(tok.split('=').next().unwrap_or(tok).to_owned());
                }
                if !more {
                    break;
                }
            }
            _ => log(display_raw(&message)).await,
        }
    }

    let mut wanted: Vec<Capability> = WANTED_CAPS.into_iter().filter(|c| offered.contains(c.as_ref())).collect();
    let want_sasl = sasl_password.is_some() && offered.contains("sasl");
    if want_sasl {
        wanted.push(Capability::Sasl);
    }
    if wanted.is_empty() || client.send_cap_req(&wanted).is_err() {
        return HashSet::new();
    }

    // Wait for the REQ to be ACKed or NAKed. Treated as atomic (one REQ ->
    // one ACK/NAK covering the whole list), which holds for the servers
    // this matters for in practice.
    let granted: HashSet<String>;
    loop {
        let Some(Ok(message)) = stream.next().await else { return HashSet::new() };
        match &message.command {
            Command::CAP(_, CapSubCommand::ACK, third, fourth) => {
                let acked: HashSet<String> = cap_arg(third, fourth).split_whitespace().map(str::to_owned).collect();
                if want_sasl && acked.contains("sasl") {
                    let _ = client.send(Command::AUTHENTICATE("PLAIN".to_owned()));
                    granted = acked;
                    break;
                }
                return acked; // nothing left to wait on
            }
            Command::CAP(_, CapSubCommand::NAK, third, fourth) => {
                let cap_list = cap_arg(third, fourth);
                let cap_list = if cap_list.is_empty() { "(unknown)" } else { cap_list };
                log(format!("server rejected capabilities: {cap_list}")).await;
                return HashSet::new();
            }
            _ => log(display_raw(&message)).await,
        }
    }

    // SASL PLAIN exchange (RFC 4616): base64("\0<authcid>\0<password>"),
    // authzid left empty since we're not authenticating as another user.
    let account = sasl_account.unwrap_or_default();
    let password = sasl_password.unwrap_or_default();
    loop {
        let Some(Ok(message)) = stream.next().await else { return granted };
        match &message.command {
            Command::AUTHENTICATE(data) if data == "+" => {
                let payload = format!("\u{0}{account}\u{0}{password}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
                let _ = client.send(Command::AUTHENTICATE(encoded));
            }
            Command::Response(Response::RPL_LOGGEDIN | Response::RPL_SASLSUCCESS, _) => return granted,
            Command::Response(
                Response::ERR_NICKLOCKED
                | Response::ERR_SASLFAIL
                | Response::ERR_SASLTOOLONG
                | Response::ERR_SASLABORT
                | Response::ERR_SASLALREADY,
                args,
            ) => {
                log(format!("SASL authentication failed: {}", args.join(" "))).await;
                return granted;
            }
            _ => log(display_raw(&message)).await,
        }
    }
}

// Formats a single message into (target, text) for display. Shared between
// the live path and batch replay (chathistory, netsplit/netjoin, unknown
// vendor batches) — `ctcp_client` is `Some` only for the live path, so a
// replayed historical CTCP request doesn't trigger a fresh auto-reply.
fn format_display(
    our_nick: &str,
    sender: &str,
    message: &Message,
    ctcp_client: Option<&Client>,
    extended_join: bool,
    chantypes: &str,
) -> (String, String) {
    match &message.command {
        Command::PRIVMSG(target, text) if text.starts_with('\u{1}') && text.ends_with('\u{1}') => {
            // A CTCP request (VERSION, PING, TIME, ...), not a chat
            // message — log it to (server) instead of spawning a tab for
            // whatever bot/client sent it.
            let ctcp = text.trim_matches('\u{1}');
            if let (Some(reply), Some(client)) = (ctcp_reply(ctcp), ctcp_client) {
                // Modeled on WeeChat's irc-ctcp.c: reply via NOTICE, wrapped
                // in \x01, and strip any embedded \x01 from what we echo
                // back (CVE-2022-2663 — a stray delimiter here can be used
                // to smuggle data past some firewalls' IRC connection
                // tracking).
                let safe_reply = reply.replace('\u{1}', " ");
                let _ = client.send(Command::NOTICE(sender.to_owned(), format!("\u{1}{safe_reply}\u{1}")));
            }
            (SERVER_TARGET.to_owned(), format!("CTCP {ctcp} from {sender}"))
        }
        Command::PRIVMSG(target, text) => {
            let target = if target == our_nick { sender } else { target.as_str() };
            (target.to_owned(), format!("<{sender}> {text}"))
        }
        Command::NOTICE(target, text) => {
            // Any non-channel NOTICE is service/informational chatter
            // (NickServ, ident/auth, CTCP replies) — route to (server) like
            // HexChat does, rather than spawning a tab per sender. Before
            // registration these can target a placeholder like "AUTH"
            // rather than our actual nick, so check the channel prefix
            // instead of comparing against our_nick. Channel NOTICEs still
            // go to their channel. Which characters mean "channel" comes
            // from the server's own CHANTYPES (ISUPPORT), not a guess —
            // most networks use "#&" but some add others.
            let target = if target.starts_with(|c| chantypes.contains(c)) { target.as_str() } else { SERVER_TARGET };
            (target.to_owned(), format!("-{sender}- {text}"))
        }
        Command::JOIN(chan, account, realname) => {
            // With extended-join granted, the crate (mis-)parses the extra
            // JOIN fields into the "key"/"password" slots positionally —
            // they're really account and realname on receive. "*" means
            // "not logged into an account". Format matches WeeChat's join
            // line: "nick [account] (realname) (user@host) has joined
            // #channel" — account/realname only when extended-join gave us
            // them, user@host whenever the prefix carries it.
            let mut text = sender.to_owned();
            if extended_join {
                if let Some(acc) = account {
                    if acc != "*" && !acc.is_empty() {
                        text.push_str(&format!(" [{acc}]"));
                    }
                }
                if let Some(rn) = realname {
                    if !rn.is_empty() {
                        text.push_str(&format!(" ({rn})"));
                    }
                }
            }
            if let Some(uh) = userhost(message) {
                text.push_str(&format!(" ({uh})"));
            }
            text.push_str(&format!(" has joined {chan}"));
            (chan.clone(), text)
        }
        Command::PART(chan, reason) => (
            chan.clone(),
            format!("{sender} left {chan}{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()),
        ),
        Command::KICK(chan, nick, reason) => (
            chan.clone(),
            format!("{sender} kicked {nick}{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()),
        ),
        Command::QUIT(reason) => (
            SERVER_TARGET.to_owned(),
            format!("{sender} quit{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()),
        ),
        Command::NICK(new_nick) => (SERVER_TARGET.to_owned(), format!("{sender} is now known as {new_nick}")),
        Command::INVITE(nick, chan) => {
            if nick == our_nick {
                (SERVER_TARGET.to_owned(), format!("{sender} invited you to {chan}"))
            } else {
                (SERVER_TARGET.to_owned(), format!("{sender} invited {nick} to {chan}"))
            }
        }
        Command::Response(Response::RPL_TOPIC, args) => (
            args.get(1).cloned().unwrap_or_else(|| SERVER_TARGET.to_owned()),
            format!("topic: {}", args.get(2).cloned().unwrap_or_default()),
        ),
        Command::Response(_, args) => (SERVER_TARGET.to_owned(), args.join(" ")),
        // FAIL/WARN/NOTE (standard-replies): not in the crate's Command
        // enum, so they arrive as Raw. Without this, errors from newer
        // specs we rely on (chathistory, multiline) would silently vanish
        // into the generic raw-line fallback instead of being legible.
        Command::Raw(cmd, args) if matches!(cmd.as_str(), "FAIL" | "WARN" | "NOTE") => {
            (SERVER_TARGET.to_owned(), format!("[{cmd}] {}", args.join(" ")))
        }
        _ => (SERVER_TARGET.to_owned(), display_raw(message)),
    }
}

// Flushes a completed batch (BATCH -id): reconstructs draft/multiline
// batches into a single joined message, and replays everything else
// (chathistory, netsplit/netjoin, unknown vendor batches) message-by-
// message through the normal display formatting — each keeps its own
// server-time tag, so chathistory backlog shows real historical
// timestamps, not "now". Membership-mutating effects (JOIN/PART/QUIT
// inside a netsplit/netjoin batch) are intentionally not replayed here;
// only display lines are — a known simplification.
async fn flush_batch(
    server_name: &str,
    our_nick: &str,
    tx: &async_channel::Sender<IrcEvent>,
    batch_type: Option<String>,
    messages: Vec<Message>,
    chantypes: &str,
) {
    if batch_type.as_deref() == Some("DRAFT/MULTILINE") {
        let mut target: Option<String> = None;
        let mut sender: Option<String> = None;
        let mut lines: Vec<String> = Vec::new();
        let mut time: Option<String> = None;
        for m in &messages {
            if let Command::PRIVMSG(t, text) = &m.command {
                let s = m.source_nickname().unwrap_or("?");
                if target.is_none() {
                    target = Some(if t == our_nick { s.to_owned() } else { t.clone() });
                    sender = Some(s.to_owned());
                    time = tag_time(m);
                }
                let concat = m.tags.as_ref().is_some_and(|tags| tags.iter().any(|tag| tag.0 == "draft/multiline-concat"));
                if concat && !lines.is_empty() {
                    lines.last_mut().unwrap().push_str(text);
                } else {
                    lines.push(text.clone());
                }
            }
        }
        if let (Some(target), Some(sender)) = (target, sender) {
            let text = format!("<{sender}> {}", lines.join("\n"));
            let _ = tx.send(IrcEvent::Line { server: server_name.to_owned(), target, text, time }).await;
        }
        return;
    }

    for m in &messages {
        let sender = m.source_nickname().unwrap_or("?");
        // extended_join doesn't matter for replay — historical JOINs are
        // rare and the account annotation isn't worth the plumbing here.
        let (target, text) = format_display(our_nick, sender, m, None, false, chantypes);
        let _ = tx.send(IrcEvent::Line { server: server_name.to_owned(), target, text, time: tag_time(m) }).await;
    }
}

async fn route_incoming(
    server_name: &str,
    client: &Client,
    message: &Message,
    tx: &async_channel::Sender<IrcEvent>,
    channel_members: &mut HashMap<String, Vec<String>>,
    names_buffer: &mut HashMap<String, Vec<String>>,
    granted_caps: &mut HashSet<String>,
    active_batches: &mut HashMap<String, (Option<String>, Vec<Message>)>,
    features: &mut ServerFeatures,
    nick_status: &mut HashMap<String, bool>,
) {
    let our_nick = client.current_nickname();
    let sender = message.source_nickname().unwrap_or("?");

    // Any message tagged as part of an in-progress batch gets buffered
    // instead of processed now — it's replayed (or reconstructed, for
    // draft/multiline) when the matching BATCH -id arrives below.
    if let Some(batch_ref) = message.tags.as_ref().and_then(|tags| tags.iter().find(|t| t.0 == "batch")).and_then(|t| t.1.clone()) {
        if let Some((_, bucket)) = active_batches.get_mut(&batch_ref) {
            bucket.push(message.clone());
            return;
        }
    }

    let send_names = |tx: &async_channel::Sender<IrcEvent>,
                       channel: &str,
                       members: &HashMap<String, Vec<String>>,
                       status: &HashMap<String, bool>| {
        let nicks: Vec<NickInfo> = members
            .get(channel)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|nick| {
                let away = status.get(&nick).copied().unwrap_or(false);
                NickInfo { nick, away }
            })
            .collect();
        let tx = tx.clone();
        let server = server_name.to_owned();
        let channel = channel.to_owned();
        async move {
            let _ = tx.send(IrcEvent::Names { server, channel, nicks }).await;
        }
    };

    // AWAY/ACCOUNT/CHGHOST (away-notify, account-notify, chghost caps)
    // aren't targeted at a channel — the server just tells us about a user,
    // and it's on us to show it in every channel we share with them.
    let msg_time = tag_time(message);
    let broadcast_to_shared = |tx: &async_channel::Sender<IrcEvent>, members: &HashMap<String, Vec<String>>, text: String| {
        let affected: Vec<String> =
            members.iter().filter(|(_, m)| m.iter().any(|n| n == sender)).map(|(chan, _)| chan.clone()).collect();
        let tx = tx.clone();
        let server = server_name.to_owned();
        let time = msg_time.clone();
        async move {
            for chan in affected {
                let _ = tx
                    .send(IrcEvent::Line { server: server.clone(), target: chan, text: text.clone(), time: time.clone() })
                    .await;
            }
        }
    };

    match &message.command {
        Command::Response(Response::RPL_ISUPPORT, args) => {
            features.apply_isupport(args);
            // No `return`: falls through to the generic Response display
            // below, same as before this arm existed.
        }
        Command::Response(Response::RPL_NAMREPLY, args) => {
            // args: [our_nick, "=", "#chan", "nick1 @nick2 +nick3 ..."]
            if let (Some(channel), Some(names)) = (args.get(2), args.last()) {
                let entry = names_buffer.entry(channel.clone()).or_default();
                entry.extend(
                    names
                        .split_whitespace()
                        // Which leading characters are rank markers (not
                        // part of the nick) comes from the server's own
                        // PREFIX (ISUPPORT), not a hardcoded guess. With
                        // userhost-in-names granted, each entry is also
                        // "nick!user@host" rather than a bare nick — strip
                        // everything from '!' onward.
                        .map(|n| {
                            let n = n.trim_start_matches(|c| features.prefixes.contains(c));
                            n.split('!').next().unwrap_or(n).to_owned()
                        }),
                );
            }
            return;
        }
        Command::Response(Response::RPL_ENDOFNAMES, args) => {
            if let Some(channel) = args.get(1) {
                let mut nicks = names_buffer.remove(channel).unwrap_or_default();
                nicks.sort_unstable();
                nicks.dedup();
                channel_members.insert(channel.clone(), nicks);
                send_names(tx, channel, channel_members, nick_status).await;
                // Backfill recent history right after joining, if the
                // server supports it — this is the whole point of
                // draft/chathistory: no more "joined and missed everything".
                if granted_caps.contains("draft/chathistory") {
                    let _ = client.send(format!("CHATHISTORY LATEST {channel} * 50").as_str());
                }
                // Ask for account/away status for everyone in the channel
                // in one shot, if the server supports it (near-universal:
                // InspIRCd, UnrealIRCd, Solanum, Ergo all do) — otherwise
                // the nicklist just shows bare names, as before.
                if features.whox {
                    // Built as Command::Raw directly, not parsed from a
                    // string: the crate's own WHO variant is
                    // WHO(Option<String>, Option<bool>) — a mask plus the
                    // legacy opers-only flag — so sending the WHOX field
                    // selector as text round-trips through that lossy
                    // shape and gets silently dropped on serialization,
                    // degrading the server to a legacy WHOREPLY. Only
                    // asking for nick+flags (not account) — WeeChat's
                    // nicklist doesn't show account either; it surfaces via
                    // extended-join/account-notify instead.
                    let _ = client.send(Command::Raw("WHO".to_owned(), vec![channel.clone(), "%nf".to_owned()]));
                }
            }
            return;
        }
        // RPL_WHOSPCRPL (354, WHOX) — not in the crate's Response enum, so
        // it arrives as Raw. args = [<our_nick>, nick, flags] given our
        // fixed "%nf" field request order (n,f).
        Command::Raw(cmd, args) if cmd == "354" => {
            if let (Some(nick), Some(flags)) = (args.get(1), args.get(2)) {
                let away = flags.starts_with('G'); // H = here, G = gone (away)
                nick_status.insert(nick.clone(), away);
            }
            return;
        }
        Command::Response(Response::RPL_ENDOFWHO, args) => {
            if let Some(channel) = args.get(1) {
                if channel_members.contains_key(channel) {
                    send_names(tx, channel, channel_members, nick_status).await;
                }
            }
            return;
        }
        Command::JOIN(chan, ..) => {
            if sender != our_nick {
                channel_members.entry(chan.clone()).or_default().push(sender.to_owned());
                send_names(tx, chan, channel_members, nick_status).await;
            }
        }
        Command::PART(chan, _) | Command::KICK(chan, _, _) => {
            let left = if let Command::KICK(_, nick, _) = &message.command { nick.as_str() } else { sender };
            if let Some(members) = channel_members.get_mut(chan) {
                members.retain(|n| n != left);
            }
            send_names(tx, chan, channel_members, nick_status).await;
        }
        Command::QUIT(_) => {
            let affected: Vec<String> = channel_members
                .iter()
                .filter(|(_, members)| members.iter().any(|n| n == sender))
                .map(|(chan, _)| chan.clone())
                .collect();
            for chan in &affected {
                if let Some(members) = channel_members.get_mut(chan) {
                    members.retain(|n| n != sender);
                }
                send_names(tx, chan, channel_members, nick_status).await;
            }
        }
        Command::NICK(new_nick) => {
            if let Some(status) = nick_status.remove(sender) {
                nick_status.insert(new_nick.clone(), status);
            }
            let affected: Vec<String> = channel_members
                .iter()
                .filter(|(_, members)| members.iter().any(|n| n == sender))
                .map(|(chan, _)| chan.clone())
                .collect();
            for chan in &affected {
                if let Some(members) = channel_members.get_mut(chan) {
                    for n in members.iter_mut() {
                        if n == sender {
                            *n = new_nick.clone();
                        }
                    }
                }
                send_names(tx, chan, channel_members, nick_status).await;
            }
        }
        Command::AWAY(reason) => {
            let text = match reason {
                Some(r) => format!("{sender} is away: {r}"),
                None => format!("{sender} is no longer away"),
            };
            nick_status.insert(sender.to_owned(), reason.is_some());
            broadcast_to_shared(tx, channel_members, text).await;
            let affected: Vec<String> =
                channel_members.iter().filter(|(_, m)| m.iter().any(|n| n == sender)).map(|(c, _)| c.clone()).collect();
            for chan in &affected {
                send_names(tx, chan, channel_members, nick_status).await;
            }
            return;
        }
        // account-notify doesn't touch nick_status (we no longer track
        // account there — see NickInfo) — it just announces the change,
        // same as WeeChat does, rather than decorating the nicklist.
        Command::ACCOUNT(account) => {
            let text = if account == "*" {
                format!("{sender} logged out")
            } else {
                format!("{sender} authenticated as {account}")
            };
            broadcast_to_shared(tx, channel_members, text).await;
            return;
        }
        Command::CHGHOST(new_user, new_host) => {
            broadcast_to_shared(tx, channel_members, format!("{sender} changed host to {new_user}@{new_host}")).await;
            return;
        }
        // cap-notify: the server can offer/withdraw capabilities after
        // registration. NEW auto-requests anything we want that we don't
        // already have; DEL/ACK/NAK here just keep granted_caps in sync
        // with reality (e.g. so echo-message dedup and chathistory-on-join
        // stay correct if a cap changes mid-session).
        Command::CAP(_, CapSubCommand::NEW, third, fourth) => {
            let want: Vec<String> = cap_arg(third, fourth)
                .split_whitespace()
                .map(|tok| tok.split('=').next().unwrap_or(tok).to_owned())
                .filter(|name| !granted_caps.contains(name) && WANTED_CAPS.iter().any(|c| c.as_ref() == name))
                .collect();
            if !want.is_empty() {
                let _ = client.send(format!("CAP REQ :{}", want.join(" ")).as_str());
            }
            return;
        }
        Command::CAP(_, CapSubCommand::DEL, third, fourth) => {
            for tok in cap_arg(third, fourth).split_whitespace() {
                granted_caps.remove(tok.split('=').next().unwrap_or(tok));
            }
            return;
        }
        Command::CAP(_, CapSubCommand::ACK, third, fourth) => {
            for tok in cap_arg(third, fourth).split_whitespace() {
                granted_caps.insert(tok.to_owned());
            }
            return;
        }
        Command::CAP(..) => return, // NAK or LS arriving post-registration (rare) — nothing to do
        Command::BATCH(reftag, subcmd, _params) => {
            if let Some(id) = reftag.strip_prefix('+') {
                active_batches.insert(id.to_owned(), (subcmd.as_ref().map(|s| s.to_str().to_owned()), Vec::new()));
            } else if let Some(id) = reftag.strip_prefix('-') {
                if let Some((batch_type, messages)) = active_batches.remove(id) {
                    flush_batch(server_name, our_nick, tx, batch_type, messages, &features.chantypes).await;
                }
            }
            return;
        }
        _ => {}
    }

    let extended_join = granted_caps.contains("extended-join");
    let (target, text) = format_display(our_nick, sender, message, Some(client), extended_join, &features.chantypes);
    let _ = tx.send(IrcEvent::Line { server: server_name.to_owned(), target, text, time: tag_time(message) }).await;
}

async fn handle_outgoing(
    server_name: &str,
    client: &Client,
    default_target: &str,
    line: &str,
    tx: &async_channel::Sender<IrcEvent>,
    granted_caps: &HashSet<String>,
) -> irc::error::Result<()> {
    let our_nick = client.current_nickname().to_owned();
    // With "echo-message" granted, the server sends our own PRIVMSG back to
    // us like any other, and route_incoming displays it — echoing locally
    // too would show every sent message twice.
    let server_echoes = granted_caps.contains("echo-message");
    let echo = |tx: &async_channel::Sender<IrcEvent>, target: &str, text: String| {
        let tx = tx.clone();
        let server = server_name.to_owned();
        let target = target.to_owned();
        async move {
            if !server_echoes {
                let _ = tx.send(IrcEvent::Line { server, target, text, time: None }).await;
            }
        }
    };

    if let Some(rest) = line.strip_prefix('/') {
        let mut parts = rest.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let arg = parts.next().unwrap_or("").trim();

        match cmd.as_str() {
            "join" if !arg.is_empty() => client.send_join(arg)?,
            "part" => {
                let target = if arg.is_empty() { default_target } else { arg };
                client.send(Command::PART(target.to_owned(), None))?;
            }
            "nick" if !arg.is_empty() => client.send(Command::NICK(arg.to_owned()))?,
            "invite" if !arg.is_empty() => {
                let mut inv_parts = arg.splitn(2, ' ');
                let nick = inv_parts.next().unwrap_or("");
                let chan = inv_parts.next().unwrap_or(default_target);
                if !nick.is_empty() {
                    client.send(Command::INVITE(nick.to_owned(), chan.to_owned()))?;
                }
            }
            "msg" => {
                let mut msg_parts = arg.splitn(2, ' ');
                if let (Some(target), Some(text)) = (msg_parts.next(), msg_parts.next()) {
                    client.send_privmsg(target, text)?;
                    echo(tx, target, format!("<{our_nick}> {text}")).await;
                }
            }
            "raw" | "quote" => client.send(arg)?,
            other => client.send_privmsg(default_target, format!("(unknown command /{other}, sent as text)"))?,
        }
    } else {
        client.send_privmsg(default_target, line)?;
        echo(tx, default_target, format!("<{our_nick}> {line}")).await;
    }
    Ok(())
}
