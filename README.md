# squawkline

IRC client written in Rust, using GTK4.

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

## Configuration

On first run, a default config is written to
`~/.config/squawkline/config.toml`. Add more `[[servers]]` entries to
connect to more than one network.
