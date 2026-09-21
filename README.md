# Thrace

A desktop Matrix client with a compact layout. Written in Rust with egui
and matrix-sdk.

Rooms, timestamps, colored nicknames, and a compact timeline. Supports
encrypted chats, device verification, replies, threads, reactions, file
uploads, and inline media. Sign in with a password or SSO.

## Build and run

You need Rust and Cargo. On x86_64 Linux, the build config also expects the
`mold` linker. Install it or change the linker in `.cargo/config.toml`.

```sh
git clone https://github.com/entailz/thrace.git
cd thrace
cargo run --release
```

For toolbar icons, install Symbols Nerd Font. The app looks for
`SymbolsNerdFont-Regular.ttf` in system font directories and
`~/.local/share/fonts/`.

Video playback uses `ffmpeg`, `ffprobe`, and `ffplay`. Make sure they are on
your `PATH` if you want in-app playback.

## Appearance

Four themes are included: `dark` (the default), `midnight`, `bbs-amber`,
and `win98`. Pick one in settings or at startup:

```sh
cargo run --release -- --theme midnight
```

Theme definitions live in `themes/` and are compiled into the binary.

## Settings

Client preferences are saved automatically in `~/.config/thrace/config.toml`,
or `$XDG_CONFIG_HOME/thrace/config.toml` if set. This includes the theme,
font size, room previews, and link preview rules, including the Twitter
preview service. The file is created on first launch.

You can edit it while Thrace is closed. Missing settings use their defaults:

```toml
theme = "dark"
font_size = 14.0
show_previews = true

[embeds]

[[embeds.rules]]
name = "X / Twitter"
enabled = false
aliases = ["x.com", "twitter.com", "vxtwitter.com", "fxtwitter.com", "fixupx.com", "fixvx.com", "nitter.net"]
open_with = "fxtwitter.com"
api = "https://api.fxtwitter.com"
```

Each embedder has its own `[[embeds.rules]]` entry. Aliases are the hostnames
it handles, `open_with` is the host used when opening links, and `api` is
the preview endpoint. Leave `api` empty to only rewrite links.

The `--theme` option overrides the saved theme for that launch. Link previews are off by
default; enable them in settings. Account preferences such as notifications
are stored on your Matrix server.

Emoji usage is stored in Matrix account data (`m.recent_emoji`), so it follows
your account across compatible clients. The picker shows your 18 most-used
emojis, with recent picks first when counts match.

## Linux notes

Wayland is used when available. File drag and drop requires X11 or XWayland:

```sh
cargo run --release -- --x11
```

You can also paste attachments with Ctrl+V.

Login tokens are stored through Secret Service, supported by KWallet and
GNOME Keyring. Unlock your wallet when prompted. If the wallet is unavailable,
Thrace keeps the login in memory and shows a retry button instead of saving
tokens to disk.

Session metadata and encryption stores live under `~/.local/share/thrace/`.
Older plaintext session files are migrated after the wallet save is verified.
The metadata file is readable only by your user and contains no tokens.

## License

AGPL-3.0-only. Bundled font credits are in [assets/README.md](assets/README.md).
