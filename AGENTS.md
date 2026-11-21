# Synth

Use the existing checkout. Cloud tasks already run in isolated environments. Create a Git worktree only when explicitly requested.

Run `cargo run --locked` for the editor and `cargo test --locked` for checks. Keep the README as it is and write source code without comments. Get approval before adding functionality or files.

Place `config.toml` beside the executable, AppImage, or `Synth.app`. The repository sample lists the defaults. Use `synth config path` to locate the active configuration and `synth config check` to validate it. The app reloads changes automatically and keeps the last valid settings on errors. Existing terminals keep their shell until the session ends. Animation direction accepts `in`, `out`, `in_out`, and `out_in`. Styles include `linear`, `sine`, `quadratic`, `cubic`, `quartic`, `quintic`, `exponential`, `circular`, `back`, `elastic`, and `bounce`. The legacy `terminal.toggle_key` and `terminal.animation_ms` apply when their newer sections are absent.

Run `synth --help` for the command list. Commands target the running copy of the same executable or package. From an external shell, invoke that copy by path when it is outside PATH. The default Windows CMD defines `synth` for the current session. Custom Windows shells can invoke the path in `SYNTH_EXE`, for example `& $env:SYNTH_EXE terminal toggle` in PowerShell.

GitHub Actions publishes the EXE, universal DMG, and x86-64 AppImage on successful main builds. The Cargo major and minor version define the series and the workflow run number supplies the patch version. Pushing a `vMAJOR.MINOR.PATCH` tag selects that exact version. Pull requests build artifacts for review.
