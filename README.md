# squawkline

IRC client written in Rust, using GTK4.

> [!WARNING]
> Work in progress. Not suitable for daily usage. Features are half-built,
> config formats will change without warning, and bugs are practically a
> core feature at this point. Compile at your own peril.

## Building from source

You'll need a Rust toolchain ([rustup.rs](https://rustup.rs)) and GTK4's
development headers.

**Ubuntu / Debian**
```
sudo apt install build-essential pkg-config libgtk-4-dev
```

**Fedora**
```
sudo dnf install gcc pkgconf-pkg-config gtk4-devel
```

**Arch Linux**
```
sudo pacman -S base-devel gtk4
```

**openSUSE**
```
sudo zypper install -t pattern devel_basis
sudo zypper install gtk4-devel
```

Then build and run:
```
cargo build --release
./target/release/squawkline
```

## Features

- Multiple simultaneous server connections, each on its own thread — a
  sidebar tree shows every server with its joined channels underneath.
- IRCv3 capability negotiation (CAP LS/REQ, `cap-notify` for capabilities
  offered mid-session), SASL PLAIN.
- `server-time`, `away-notify`, `account-notify`, `chghost`,
  `echo-message`, `extended-join`, `invite-notify`, `multi-prefix`,
  `message-tags`.
- `draft/chathistory`: automatically backfills recent history when
  joining a channel, on networks that support it.
- `draft/multiline`: reconstructed into a single message.
- `standard-replies` (FAIL/WARN/NOTE) shown legibly instead of vanishing.
- RPL_ISUPPORT (CHANTYPES, PREFIX) actually respected — channel-vs-private
  detection and nicklist rank-marker stripping use what the server
  reports rather than a hardcoded guess.
- CTCP auto-replies (VERSION, PING, TIME, CLIENTINFO, SOURCE).
- A nicklist per channel.
- Commands: `/join`, `/part`, `/nick`, `/msg`, `/invite`, `/raw`.

## Configuration

On first run, a default config is written to
`~/.config/squawkline/config.toml`. Add more `[[servers]]` entries to
connect to more than one network:

```toml
[[servers]]
name = "libera"
nickname = "yournick"
server = "irc.libera.chat"
use_tls = true
channels = ["#somechannel"]
# Optional SASL PLAIN — only attempted if the server offers "sasl".
# sasl_account = "yournick"   # defaults to `nickname` if omitted
# sasl_password = "..."

[[servers]]
name = "another-network"
nickname = "yournick"
server = "irc.example.org"
port = 6697
use_tls = true
channels = ["#chan1", "#chan2"]
```
