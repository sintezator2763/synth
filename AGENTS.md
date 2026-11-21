# Synth

Use the existing checkout. Cloud tasks already run in isolated environments. Create a Git worktree only when explicitly requested.

Run `cargo run --locked` for the editor and `cargo test --locked` for checks. Keep the README as it is and write source code without comments. Get approval before adding functionality or files.

Synth creates its config on first GUI launch. Windows stores it beside the executable, Linux in `$XDG_CONFIG_HOME/synth/config.toml` or `~/.config/synth/config.toml`, and macOS in `~/Library/Application Support/Synth/config.toml`. On Linux and macOS an existing adjacent config is copied when the new location is empty. Use `synth config path` and `synth config check`. Bindings are read from the config, including Esc in the generated sample. An empty `[keybindings]` table disables Synth shortcuts. The app reloads changes automatically and keeps the last valid settings on errors. Existing terminals keep their shell until the session ends. Animation direction accepts `in`, `out`, `in_out`, and `out_in`. Styles include `linear`, `sine`, `quadratic`, `cubic`, `quartic`, `quintic`, `exponential`, `circular`, `back`, `elastic`, and `bounce`. The legacy `terminal.toggle_key` and `terminal.animation_ms` apply when their newer sections are absent.

Run `synth --help` for the command list. Commands target the running copy of the same executable or package. From an external shell, invoke that copy by path when it is outside PATH. The default Windows CMD defines `synth` for the current session. Custom Windows shells can invoke the path in `SYNTH_EXE`, for example `& $env:SYNTH_EXE terminal toggle` in PowerShell.

The built-in terminal supports direct Kitty graphics transfers for PNG, RGB and RGBA, including chunked transfers and zlib compression. At a Unix PTY, Yazi can detect the streamed Kitty protocol for previews and Fastfetch can use it with `fastfetch --kitty IMAGE`. Windows ConPTY does not transparently forward Kitty APC sequences. With the panel open, `synth terminal image PATH` displays a local PNG or JPEG through Synth's command channel.

GitHub Actions publishes the EXE, universal DMG, and x86-64 AppImage on successful main builds. The Cargo major and minor version define the series and the workflow run number supplies the patch version. Pushing a `vMAJOR.MINOR.PATCH` tag selects that exact version. Pull requests build artifacts for review.
