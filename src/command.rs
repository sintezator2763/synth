use std::{
    collections::hash_map::DefaultHasher,
    ffi::{OsStr, OsString},
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    num::NonZeroU32,
    path::Path,
    sync::mpsc,
    time::Duration,
};

use eframe::egui;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream, prelude::*,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const WINDOW_ENV: &str = "SYNTH_WINDOW_PID";

fn socket_name(installation: &Path, window: Option<NonZeroU32>) -> io::Result<Name<'static>> {
    let mut hash = DefaultHasher::new();
    installation.hash(&mut hash);
    #[cfg(unix)]
    unsafe {
        libc::geteuid().hash(&mut hash);
    }
    let name = match window {
        Some(pid) => format!("synth-{:016x}-{pid}", hash.finish()),
        None => format!("synth-{:016x}", hash.finish()),
    };
    if GenericNamespaced::is_supported() {
        name.to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir()
            .join(name)
            .into_os_string()
            .to_fs_name::<GenericFilePath>()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Zoom {
    In,
    Out,
    Reset,
}

impl Zoom {
    fn parse(action: &str) -> Result<Self, String> {
        match action {
            "in" => Ok(Self::In),
            "out" => Ok(Self::Out),
            "reset" => Ok(Self::Reset),
            _ => Err("Expected zoom in, out, or reset".into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Open(OsString),
    New,
    Save,
    SaveAs(OsString),
    Reload,
    Goto { line: usize, column: usize },
    Close { discard: bool },
    Properties,
    Tabs,
    NextTab,
    PreviousTab,
    Find(String),
    FindNext,
    ToggleSearch,
    CloseSearch,
    SearchNext,
    CancelDialog,
    ToggleWrap,
    ToggleTerminal,
    ToggleEditorTerminal,
    CopyTerminalSelection,
    TerminalZoom(Zoom),
    InterfaceZoom(Zoom),
    HideTerminal,
    TerminalImage(OsString),
    ToggleFocus,
    ConfigPath,
    ConfigCheck,
    ConfigReload,
}

pub const USAGE: &str = "Usage: synth COMMAND [ARGS]\nRun 'synth help' for a list of commands.";

pub fn help() -> String {
    use std::fmt::Write as _;

    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "DOCUMENTS",
            &[
                ("new", "Create an untitled document"),
                ("open PATH", "Open a file or switch to it"),
                ("save", "Save the active document"),
                ("saveas PATH", "Save the active document to PATH"),
                ("reload", "Reload the active document from disk"),
                ("goto LINE[:COLUMN]", "Move the cursor to a line and column"),
                ("wrap toggle", "Toggle line wrapping in the active document"),
                ("close", "Close an unchanged document"),
                ("close --discard", "Close without saving changes"),
                ("properties", "Show file information"),
            ],
        ),
        (
            "TABS",
            &[
                ("tab list", "List all open documents"),
                ("tab next", "Switch to the next tab"),
                ("tab prev", "Switch to the previous tab"),
            ],
        ),
        (
            "SEARCH",
            &[
                ("find TEXT", "Find the first occurrence"),
                ("find next", "Find the next occurrence"),
                ("find -- next", "Find the literal word \"next\""),
                ("search toggle", "Show or hide the search and replace bar"),
                ("search close", "Close the search and replace bar"),
                ("search next", "Go to the next match in the search bar"),
            ],
        ),
        (
            "TERMINAL",
            &[
                ("terminal toggle", "Show or hide the terminal"),
                (
                    "terminal toggle-edit",
                    "Toggle the terminal from the editor",
                ),
                ("terminal copy", "Copy the terminal selection"),
                (
                    "terminal zoom ACTION",
                    "Change terminal font size (in, out, reset)",
                ),
                ("terminal hide", "Hide the terminal panel"),
                (
                    "terminal image PATH",
                    "Display a PNG or JPEG in the terminal",
                ),
                ("focus toggle", "Switch focus between editor and terminal"),
            ],
        ),
        (
            "CONFIGURATION",
            &[
                ("config path", "Print the configuration path"),
                ("config check", "Validate the configuration"),
                ("config reload", "Reload configuration from disk"),
            ],
        ),
        (
            "GENERAL",
            &[
                ("dialog cancel", "Dismiss the current confirmation dialog"),
                ("zoom ACTION", "Scale the whole interface (in, out, reset)"),
                ("help", "Display this help"),
                ("-h, --help", "Display this help"),
            ],
        ),
    ];
    let mut output =
        String::from("Synth - terminal-driven text editor\n\nUsage: synth COMMAND [ARGS]\n\n");
    for &(title, commands) in SECTIONS {
        output.push_str(title);
        output.push('\n');
        for &(command, description) in commands {
            writeln!(output, "  {command:<22}  {description}").unwrap();
        }
        output.push('\n');
    }
    output.push_str(
        "Configure shortcuts in [keybinds] in config.toml.\nUse 'synth PATH' or 'synth open PATH' to open a file, starting Synth if needed. Most other commands require a running window.\n",
    );
    output
}

impl Command {
    pub fn parse(args: &[OsString], directory: &Path) -> Result<Self, String> {
        let word = |index: usize| args.get(index).and_then(|arg| arg.to_str()).unwrap_or("");
        match (args.len(), word(0), word(1)) {
            (2.., "open", _) => path_argument(directory, &args[1..]).map(Self::Open),
            (1, "new", _) => Ok(Self::New),
            (1, "save", _) => Ok(Self::Save),
            (1, "reload", _) => Ok(Self::Reload),
            (2, "goto", location) => {
                let (line, column) = location.split_once(':').unwrap_or((location, "1"));
                let positive =
                    |value: &str| value.parse::<usize>().ok().filter(|&number| number > 0);
                match (positive(line), positive(column)) {
                    (Some(line), Some(column)) => Ok(Self::Goto { line, column }),
                    _ => Err("Expected goto LINE[:COLUMN] with positive numbers".into()),
                }
            }
            (2.., "saveas", _) => path_argument(directory, &args[1..]).map(Self::SaveAs),
            (1, "close", _) => Ok(Self::Close { discard: false }),
            (2, "close", "-d" | "--discard") => Ok(Self::Close { discard: true }),
            (1, "properties", _) => Ok(Self::Properties),
            (2, "tab", "list") => Ok(Self::Tabs),
            (2, "tab", "next") => Ok(Self::NextTab),
            (2, "tab", "prev") => Ok(Self::PreviousTab),
            (2, "search", "toggle") => Ok(Self::ToggleSearch),
            (2, "search", "close") => Ok(Self::CloseSearch),
            (2, "search", "next") => Ok(Self::SearchNext),
            (2, "dialog", "cancel") => Ok(Self::CancelDialog),
            (2, "wrap", "toggle") => Ok(Self::ToggleWrap),
            (2, "find", "next") => Ok(Self::FindNext),
            (2, "find", text) if !text.is_empty() => Ok(Self::Find(text.into())),
            (3, "find", "--") if !word(2).is_empty() => Ok(Self::Find(word(2).into())),
            (2, "terminal", "toggle") => Ok(Self::ToggleTerminal),
            (2, "terminal", "toggle-edit") => Ok(Self::ToggleEditorTerminal),
            (2, "terminal", "copy") => Ok(Self::CopyTerminalSelection),
            (3, "terminal", "zoom") => Zoom::parse(word(2)).map(Self::TerminalZoom),
            (2, "zoom", action) => Zoom::parse(action).map(Self::InterfaceZoom),
            (2, "terminal", "hide") => Ok(Self::HideTerminal),
            (3.., "terminal", "image") => {
                path_argument(directory, &args[2..]).map(Self::TerminalImage)
            }
            (2, "focus", "toggle") => Ok(Self::ToggleFocus),
            (2, "config", "path") => Ok(Self::ConfigPath),
            (2, "config", "check") => Ok(Self::ConfigCheck),
            (2, "config", "reload") => Ok(Self::ConfigReload),
            _ => Err(USAGE.into()),
        }
    }

    pub fn parse_cli(args: &[OsString], directory: &Path) -> Result<Self, String> {
        if let [path] = args
            && !path.is_empty()
            && !path.as_encoded_bytes().starts_with(b"-")
            && directory.join(path).is_file()
        {
            return Self::parse(args, directory)
                .or_else(|_| path_argument(directory, args).map(Self::Open));
        }
        Self::parse(args, directory)
    }
}

fn path_argument(directory: &Path, parts: &[OsString]) -> Result<OsString, String> {
    let path = parts.join(OsStr::new(" "));
    if path.is_empty() {
        return Err("A file path is required".into());
    }
    Ok(directory.join(path).into_os_string())
}

pub type Reply = Result<String, String>;

pub struct Request {
    pub command: Command,
    pub respond: mpsc::SyncSender<Reply>,
}

const MAX_FRAME: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

fn receive<T: DeserializeOwned>(stream: &mut impl Read) -> io::Result<T> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Synth message is too large",
        ));
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}

fn send<T: Serialize>(stream: &mut impl Write, value: &T) -> io::Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Synth message is too large",
        ));
    }
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(&data)
}

#[cfg(unix)]
fn check_peer(stream: &Stream) -> io::Result<()> {
    let user = unsafe { libc::geteuid() };
    if stream.peer_creds()?.euid() == Some(user) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Synth belongs to another user",
        ))
    }
}

fn listener_options(name: Name<'static>) -> io::Result<ListenerOptions<'static>> {
    let options = ListenerOptions::new().name(name);
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::{
            local_socket::ListenerOptionsExt, security_descriptor::SecurityDescriptor,
        };
        options.security_descriptor(SecurityDescriptor::deserialize(widestring::u16cstr!(
            "D:P(A;;GA;;;SY)(A;;GA;;;OW)"
        ))?)
    };
    Ok(options)
}

pub fn dispatch(installation: &Path, command: &Command) -> Reply {
    dispatch_if_running(installation, command)?
        .ok_or_else(|| "Could not connect to the running Synth: no command listener".to_owned())?
}

pub fn dispatch_if_running(
    installation: &Path,
    command: &Command,
) -> Result<Option<Reply>, String> {
    let window = match std::env::var(WINDOW_ENV) {
        Ok(value) if value.is_empty() => None,
        Ok(value) => Some(
            value
                .parse::<NonZeroU32>()
                .map_err(|_| "Invalid SYNTH_WINDOW_PID")?,
        ),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err("Invalid SYNTH_WINDOW_PID".into()),
    };
    let name = socket_name(installation, window).map_err(|error| crate::io_message(&error))?;
    let mut retry = window.is_none();
    let mut stream = loop {
        match Stream::connect(name.clone()) {
            Ok(stream) => break stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                if retry {
                    retry = false;
                    std::thread::sleep(SHARED_RETRY * 2);
                    continue;
                }
                return match window {
                    Some(pid) => Err(format!("Could not connect to Synth window {pid}")),
                    None => Ok(None),
                };
            }
            Err(error) => {
                return Err(format!(
                    "Could not connect to the running Synth: {}",
                    crate::io_message(&error)
                ));
            }
        }
    };
    exchange(&mut stream, command)
        .map(Some)
        .map_err(|error| crate::io_message(&error))
}

fn exchange(stream: &mut Stream, command: &Command) -> io::Result<Reply> {
    #[cfg(unix)]
    check_peer(stream)?;
    write_frame(stream, command)?;
    read_frame(stream, COMMAND_TIMEOUT + Duration::from_secs(1))
}

const SHARED_RETRY: Duration = Duration::from_millis(150);

pub fn listen(installation: &Path, ctx: &egui::Context) -> mpsc::Receiver<Request> {
    let (sender, receiver) = mpsc::channel();
    for window in [NonZeroU32::new(std::process::id()), None] {
        match listen_at(installation, window, ctx.clone(), sender.clone()) {
            Ok(()) => {}
            Err(error) if window.is_none() && address_taken(&error) => {
                let installation = installation.to_owned();
                let ctx = ctx.clone();
                let sender = sender.clone();
                std::thread::spawn(move || {
                    loop {
                        std::thread::sleep(SHARED_RETRY);
                        match listen_at(&installation, None, ctx.clone(), sender.clone()) {
                            Err(error) if address_taken(&error) => continue,
                            Err(error) => {
                                eprintln!("Synth command listener: {}", crate::io_message(&error))
                            }
                            Ok(()) => {}
                        }
                        break;
                    }
                });
            }
            Err(error) => eprintln!("Synth command listener: {}", crate::io_message(&error)),
        }
    }
    receiver
}

fn address_taken(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AddrInUse
        || (cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied)
}

fn listen_at(
    installation: &Path,
    window: Option<NonZeroU32>,
    ctx: egui::Context,
    sender: mpsc::Sender<Request>,
) -> io::Result<()> {
    let name = socket_name(installation, window)?;
    let listener = match listener_options(name.clone())?.create_sync() {
        Err(error)
            if !GenericNamespaced::is_supported()
                && error.kind() == io::ErrorKind::AddrInUse
                && Stream::connect(name.clone())
                    .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionRefused) =>
        {
            listener_options(name)?
                .try_overwrite(true)
                .max_spin_time(Duration::from_millis(100))
                .create_sync()?
        }
        result => result?,
    };
    std::thread::Builder::new()
        .name("synth-command".into())
        .spawn(move || {
            while let Ok(mut stream) = listener.accept() {
                let _ = handle_request(&mut stream, &sender, &ctx);
            }
        })?;
    Ok(())
}

fn handle_request(
    stream: &mut Stream,
    sender: &mpsc::Sender<Request>,
    ctx: &egui::Context,
) -> io::Result<()> {
    #[cfg(unix)]
    check_peer(stream)?;
    let command = read_frame(stream, Duration::from_millis(500))?;
    let (respond, response) = mpsc::sync_channel(1);
    sender
        .send(Request { command, respond })
        .map_err(io::Error::other)?;
    ctx.request_repaint();
    let reply = response
        .recv_timeout(COMMAND_TIMEOUT)
        .unwrap_or_else(|_| Err("Synth did not complete the command in time".into()));
    write_frame(stream, &reply)
}

fn read_frame<T: DeserializeOwned>(stream: &mut Stream, timeout: Duration) -> io::Result<T> {
    #[cfg(unix)]
    {
        stream.set_recv_timeout(Some(timeout))?;
        receive(stream)
    }
    #[cfg(windows)]
    {
        stream.set_nonblocking(true)?;
        receive(&mut PipeIo {
            stream,
            deadline: std::time::Instant::now() + timeout,
        })
    }
}

fn write_frame<T: Serialize>(stream: &mut Stream, value: &T) -> io::Result<()> {
    let timeout = Duration::from_secs(1);
    #[cfg(unix)]
    {
        stream.set_send_timeout(Some(timeout))?;
        send(stream, value)
    }
    #[cfg(windows)]
    {
        stream.set_nonblocking(true)?;
        send(
            &mut PipeIo {
                stream,
                deadline: std::time::Instant::now() + timeout,
            },
            value,
        )
    }
}

#[cfg(windows)]
struct PipeIo<'a> {
    stream: &'a mut Stream,
    deadline: std::time::Instant,
}

#[cfg(windows)]
impl PipeIo<'_> {
    fn retry(
        &mut self,
        mut operation: impl FnMut(&mut Stream) -> io::Result<usize>,
    ) -> io::Result<usize> {
        loop {
            match operation(self.stream) {
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error()
                            == Some(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32) => {}
                result => return result,
            }
            let remaining = self
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(5)));
        }
    }
}

#[cfg(windows)]
impl Read for PipeIo<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        if buffer.is_empty() {
            return Ok(0);
        }
        self.retry(|stream| {
            let Stream::NamedPipe(pipe) = &*stream;
            let mut available = 0;
            let ok = unsafe {
                PeekNamedPipe(
                    pipe.as_handle().as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if available == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let size = buffer.len().min(available as usize);
            stream.read(&mut buffer[..size])
        })
    }
}

#[cfg(windows)]
impl Write for PipeIo<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.retry(|stream| match stream.write(buffer) {
            Ok(0) if !buffer.is_empty() => Err(io::ErrorKind::WouldBlock.into()),
            result => result,
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lists_commands_in_aligned_columns() {
        let printed = help();
        for command in [
            "new",
            "open PATH",
            "saveas PATH",
            "close --discard",
            "tab next",
            "find -- next",
            "terminal image PATH",
            "config check",
            "-h, --help",
        ] {
            assert!(
                printed
                    .lines()
                    .any(|line| line.starts_with(&format!("  {command}")))
            );
        }
        let rows = printed
            .lines()
            .filter(|line| line.starts_with("  "))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 33);
        for row in rows {
            assert_eq!(&row[24..26], "  ");
        }
    }

    #[test]
    fn goto_parses_one_based_positions() {
        let parse =
            |location: &str| Command::parse(&["goto".into(), location.into()], Path::new("."));
        assert!(matches!(
            parse("8"),
            Ok(Command::Goto { line: 8, column: 1 })
        ));
        assert!(matches!(
            parse("8:3"),
            Ok(Command::Goto { line: 8, column: 3 })
        ));
        for invalid in ["0", "8:0", "8:", "8:3:2"] {
            assert!(parse(invalid).is_err());
        }
    }

    #[test]
    fn open_joins_unquoted_path_words_and_preserves_one_quoted_argument() {
        let root = Path::new("documents");
        let words = ["open", "Project", "Notes.txt"].map(OsString::from);
        assert!(matches!(
            Command::parse(&words, root),
            Ok(Command::Open(path)) if path == root.join("Project Notes.txt").into_os_string()
        ));
        let quoted = ["open", "Project  Notes.txt"].map(OsString::from);
        assert!(matches!(
            Command::parse(&quoted, root),
            Ok(Command::Open(path)) if path == root.join("Project  Notes.txt").into_os_string()
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_rooted_forward_slash_path_keeps_the_callers_drive() {
        let root = Path::new("C:/Users/Someone");
        let words = ["open", "/Work/Project", "Notes.txt"].map(OsString::from);
        assert!(matches!(
            Command::parse(&words, root),
            Ok(Command::Open(path)) if Path::new(&path) == Path::new("C:/Work/Project Notes.txt")
        ));
    }

    #[test]
    fn unicode_open_and_close_round_trip_over_local_transport() {
        let directory = tempfile::tempdir().unwrap();
        let installation = directory.path().join("synth");
        let path = directory.path().join("caf\u{e9} file.txt").into_os_string();
        let requests = listen(&installation, &egui::Context::default());
        let server = std::thread::spawn(move || {
            let request = requests.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(matches!(request.command, Command::Open(opened) if opened == path));
            request.respond.send(Ok("Opened".into())).unwrap();
            let request = requests.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(matches!(request.command, Command::Close { discard: true }));
            request
                .respond
                .send(Err("Expected error reply".into()))
                .unwrap();
        });
        let connect =
            |window| Stream::connect(socket_name(&installation, window).unwrap()).unwrap();
        let mut window = connect(NonZeroU32::new(std::process::id()));
        assert_eq!(
            exchange(
                &mut window,
                &Command::Open(directory.path().join("caf\u{e9} file.txt").into_os_string())
            )
            .unwrap(),
            Ok("Opened".into())
        );
        let mut shared = connect(None);
        assert_eq!(
            exchange(&mut shared, &Command::Close { discard: true }).unwrap(),
            Err("Expected error reply".into())
        );
        server.join().unwrap();
    }
}
