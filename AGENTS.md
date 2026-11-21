# Synth

Use the existing checkout. Cloud tasks already run in isolated environments. Create a Git worktree only when explicitly requested.

Run `cargo run --locked` for the editor and `cargo test --locked` for checks. Keep the README as it is and write source code without comments. Get approval before adding functionality or files.

Copy `config.toml` beside the executable to customize a development build. For packaged builds, place it beside `synth.exe`, the AppImage, or `Synth.app`. Synth uses built-in defaults when the file is absent and never writes it. Supported settings include the editor theme and font size, terminal side, width, animation, font size, shortcut, shell executable and arguments. Set `toggle_key = ""` to disable the shortcut. The default shell is CMD on Windows and the user's shell on Unix.

Press Esc to open or close the terminal. `synth terminal toggle` addresses the running copy of the same executable or package. From an external shell, invoke that copy by path when it is outside PATH. Keep the portable Windows binary named `synth.exe` to use the short command from the embedded shell.

GitHub Actions builds on changes to main and pull requests. Pushing a tag matching the Cargo version, such as `v0.1.0`, publishes the EXE, universal DMG, and x86-64 AppImage.
