use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use base64::Engine;
use futures::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Entry, Label, ListBox, ListBoxRow,
    Orientation, ScrolledWindow, SelectionMode, TextBuffer, TextView,
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
    // A line to append to the `server`/`target` buffer.
    Line { server: String, target: String, text: String },
    // The full, current member list for `server`/`channel` (replaces, not merges).
    Names { server: String, channel: String, nicks: Vec<String> },
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

    let text_view = TextView::builder().editable(false).build();
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
    let channel_nicks: Rc<RefCell<HashMap<String, Vec<String>>>> = Rc::new(RefCell::new(HashMap::new()));
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
                for nick in nicks {
                    let row = ListBoxRow::new();
                    row.set_child(Some(&Label::new(Some(nick))));
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
                IrcEvent::Line { server, target, text } => {
                    let buf = get_or_create_buffer(&server, &target);
                    let mut end = buf.end_iter();
                    buf.insert(&mut end, &format!("[{}] {}\n", timestamp(), text));

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
            let _ = tx.send(IrcEvent::Line { server, target, text }).await;
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
    negotiate(&client, &server_name, sasl_account.as_deref(), sasl_password.as_deref(), &mut stream, &tx).await;

    if let Err(e) = client.identify() {
        send(SERVER_TARGET, format!("identify error: {e}")).await;
        return;
    }

    // Members-per-channel, kept in sync via NAMES replies and JOIN/PART/
    // QUIT/KICK/NICK so the UI's nicklist can just mirror this on change.
    let mut channel_members: HashMap<String, Vec<String>> = HashMap::new();
    // Accumulates RPL_NAMREPLY (353) lines until RPL_ENDOFNAMES (366).
    let mut names_buffer: HashMap<String, Vec<String>> = HashMap::new();

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming.transpose().unwrap_or(None) {
                    Some(message) => {
                        route_incoming(&server_name, &client, &message, &tx, &mut channel_members, &mut names_buffer).await;
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
                if let Err(e) = handle_outgoing(&server_name, &client, &default_target, text, &tx).await {
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
const WANTED_CAPS: [Capability; 10] = [
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
];

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
) {
    let log = |text: String| {
        let tx = tx.clone();
        let server = server_name.to_owned();
        async move {
            let _ = tx.send(IrcEvent::Line { server, target: SERVER_TARGET.to_owned(), text }).await;
        }
    };

    if client.send_cap_ls(NegotiationVersion::V302).is_err() {
        return;
    }

    // Accumulate CAP LS (possibly multi-line, continued via a "*" marker
    // under IRCv3.2) until the server signals it's done listing.
    let mut offered: HashSet<String> = HashSet::new();
    loop {
        let Some(Ok(message)) = stream.next().await else { return };
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
        return;
    }

    // Wait for the REQ to be ACKed or NAKed. Treated as atomic (one REQ ->
    // one ACK/NAK covering the whole list), which holds for the servers
    // this matters for in practice.
    loop {
        let Some(Ok(message)) = stream.next().await else { return };
        match &message.command {
            Command::CAP(_, CapSubCommand::ACK, _, param) => {
                let granted = param.as_deref().unwrap_or("");
                if want_sasl && granted.split_whitespace().any(|c| c == "sasl") {
                    let _ = client.send(Command::AUTHENTICATE("PLAIN".to_owned()));
                    break;
                }
                return; // nothing left to wait on
            }
            Command::CAP(_, CapSubCommand::NAK, _, param) => {
                log(format!("server rejected capabilities: {}", param.as_deref().unwrap_or("(unknown)"))).await;
                return;
            }
            _ => log(display_raw(&message)).await,
        }
    }

    // SASL PLAIN exchange (RFC 4616): base64("\0<authcid>\0<password>"),
    // authzid left empty since we're not authenticating as another user.
    let account = sasl_account.unwrap_or_default();
    let password = sasl_password.unwrap_or_default();
    loop {
        let Some(Ok(message)) = stream.next().await else { return };
        match &message.command {
            Command::AUTHENTICATE(data) if data == "+" => {
                let payload = format!("\u{0}{account}\u{0}{password}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
                let _ = client.send(Command::AUTHENTICATE(encoded));
            }
            Command::Response(Response::RPL_LOGGEDIN | Response::RPL_SASLSUCCESS, _) => return,
            Command::Response(
                Response::ERR_NICKLOCKED
                | Response::ERR_SASLFAIL
                | Response::ERR_SASLTOOLONG
                | Response::ERR_SASLABORT
                | Response::ERR_SASLALREADY,
                args,
            ) => {
                log(format!("SASL authentication failed: {}", args.join(" "))).await;
                return;
            }
            _ => log(display_raw(&message)).await,
        }
    }
}

async fn route_incoming(
    server_name: &str,
    client: &Client,
    message: &Message,
    tx: &async_channel::Sender<IrcEvent>,
    channel_members: &mut HashMap<String, Vec<String>>,
    names_buffer: &mut HashMap<String, Vec<String>>,
) {
    let our_nick = client.current_nickname();
    let sender = message.source_nickname().unwrap_or("?");

    let send_names = |tx: &async_channel::Sender<IrcEvent>, channel: &str, members: &HashMap<String, Vec<String>>| {
        let nicks = members.get(channel).cloned().unwrap_or_default();
        let tx = tx.clone();
        let server = server_name.to_owned();
        let channel = channel.to_owned();
        async move {
            let _ = tx.send(IrcEvent::Names { server, channel, nicks }).await;
        }
    };

    match &message.command {
        Command::Response(Response::RPL_NAMREPLY, args) => {
            // args: [our_nick, "=", "#chan", "nick1 @nick2 +nick3 ..."]
            if let (Some(channel), Some(names)) = (args.get(2), args.last()) {
                let entry = names_buffer.entry(channel.clone()).or_default();
                entry.extend(
                    names
                        .split_whitespace()
                        .map(|n| n.trim_start_matches(['@', '+', '%', '~', '&']).to_owned()),
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
                send_names(tx, channel, channel_members).await;
            }
            return;
        }
        Command::JOIN(chan, ..) => {
            if sender != our_nick {
                channel_members.entry(chan.clone()).or_default().push(sender.to_owned());
                send_names(tx, chan, channel_members).await;
            }
        }
        Command::PART(chan, _) | Command::KICK(chan, _, _) => {
            let left = if let Command::KICK(_, nick, _) = &message.command { nick.as_str() } else { sender };
            if let Some(members) = channel_members.get_mut(chan) {
                members.retain(|n| n != left);
            }
            send_names(tx, chan, channel_members).await;
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
                send_names(tx, chan, channel_members).await;
            }
        }
        Command::NICK(new_nick) => {
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
                send_names(tx, chan, channel_members).await;
            }
        }
        _ => {}
    }

    let (target, text) = match &message.command {
        Command::PRIVMSG(target, text) if text.starts_with('\u{1}') && text.ends_with('\u{1}') => {
            // A CTCP request (VERSION, PING, TIME, ...), not a chat
            // message — log it to (server) instead of spawning a tab for
            // whatever bot/client sent it.
            let ctcp = text.trim_matches('\u{1}');
            if let Some(reply) = ctcp_reply(ctcp) {
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
            let target = if target == our_nick { sender } else { target };
            (target.to_owned(), format!("<{sender}> {text}"))
        }
        Command::NOTICE(target, text) => {
            // Any non-channel NOTICE is service/informational chatter
            // (NickServ, ident/auth, CTCP replies) — route to (server) like
            // HexChat does, rather than spawning a tab per sender. Before
            // registration these can target a placeholder like "AUTH"
            // rather than our actual nick, so check the channel prefix
            // instead of comparing against our_nick. Channel NOTICEs still
            // go to their channel.
            let target = if target.starts_with(['#', '&', '!', '+']) { target } else { SERVER_TARGET };
            (target.to_owned(), format!("-{sender}- {text}"))
        }
        Command::JOIN(chan, ..) => (chan.clone(), format!("{sender} joined {chan}")),
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
        Command::Response(Response::RPL_TOPIC, args) => (
            args.get(1).cloned().unwrap_or_else(|| SERVER_TARGET.to_owned()),
            format!("topic: {}", args.get(2).cloned().unwrap_or_default()),
        ),
        Command::Response(_, args) => (SERVER_TARGET.to_owned(), args.join(" ")),
        _ => (SERVER_TARGET.to_owned(), display_raw(message)),
    };

    let _ = tx.send(IrcEvent::Line { server: server_name.to_owned(), target, text }).await;
}

async fn handle_outgoing(
    server_name: &str,
    client: &Client,
    default_target: &str,
    line: &str,
    tx: &async_channel::Sender<IrcEvent>,
) -> irc::error::Result<()> {
    let our_nick = client.current_nickname().to_owned();
    let echo = |tx: &async_channel::Sender<IrcEvent>, target: &str, text: String| {
        let tx = tx.clone();
        let server = server_name.to_owned();
        let target = target.to_owned();
        async move {
            let _ = tx.send(IrcEvent::Line { server, target, text }).await;
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
