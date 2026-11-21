# Synth

Use the existing checkout. Cloud tasks already run in isolated environments. Create a Git worktree only when explicitly requested.

Run `cargo run --locked` for the editor and `cargo test --locked` for checks. Keep the README as it is and write source code without comments. Get approval before adding functionality or files.

Copy `config.toml` beside the executable to customize a development build. For packaged builds, place it beside `synth.exe`, the AppImage, or `Synth.app`. Synth uses built-in defaults when the file is absent and never writes it. Supported settings include the editor theme and font size, terminal side, width, animation, font size, shortcut, shell executable and arguments. Set `toggle_key = ""` to disable the shortcut. The default shell is CMD on Windows and the user's shell on Unix.

Press Esc to open or close the terminal. `synth terminal toggle` addresses the running copy of the same executable or package. From an external shell, invoke that copy by path when it is outside PATH. The default Windows CMD defines `synth` for the current session. Custom Windows shells can invoke the path in `SYNTH_EXE`, for example `& $env:SYNTH_EXE terminal toggle` in PowerShell.

GitHub Actions publishes the EXE, universal DMG, and x86-64 AppImage on successful main builds. The Cargo major and minor version define the series and the workflow run number supplies the patch version. Pushing a `vMAJOR.MINOR.PATCH` tag selects that exact version. Pull requests build artifacts for review.
