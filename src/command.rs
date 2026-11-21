use std::{
    collections::hash_map::DefaultHasher,
    ffi::{OsStr, OsString},
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::Path,
    sync::mpsc,
    time::Duration,
};

use eframe::egui;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name, Stream, prelude::*,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

fn socket_name(installation: &Path) -> io::Result<Name<'static>> {
    let mut hash = DefaultHasher::new();
    installation.hash(&mut hash);
    #[cfg(unix)]
    unsafe {
        libc::geteuid().hash(&mut hash);
    }
    let name = format!("synth-{:016x}", hash.finish());
    if GenericNamespaced::is_supported() {
        name.to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir()
            .join(name)
            .into_os_string()
            .to_fs_name::<GenericFilePath>()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Open(OsString),
    New,
    Save,
    SaveAs(OsString),
    Close { discard: bool },
    Properties,
    Tabs,
    NextTab,
    PreviousTab,
    Find(String),
    FindNext,
    ToggleTerminal,
    TerminalImage(OsString),
    ToggleFocus,
    ConfigPath,
    ConfigCheck,
    ConfigReload,
}

pub const USAGE: &str = "Usage: synth [open PATH | new | save | saveas PATH | close [-d|--discard] | properties | tab list|next|prev | find TEXT|next | terminal toggle|image PATH | focus toggle | config path|check|reload]";

impl Command {
    pub fn parse(args: &[OsString], directory: &Path) -> Result<Self, String> {
        let word = |index: usize| args.get(index).and_then(|arg| arg.to_str()).unwrap_or("");
        match (args.len(), word(0), word(1)) {
            (2.., "open", _) => path_argument(directory, &args[1..]).map(Self::Open),
            (1, "new", _) => Ok(Self::New),
            (1, "save", _) => Ok(Self::Save),
            (2.., "saveas", _) => path_argument(directory, &args[1..]).map(Self::SaveAs),
            (1, "close", _) => Ok(Self::Close { discard: false }),
            (2, "close", "-d" | "--discard") => Ok(Self::Close { discard: true }),
            (1, "properties", _) => Ok(Self::Properties),
            (2, "tab", "list") => Ok(Self::Tabs),
            (2, "tab", "next") => Ok(Self::NextTab),
            (2, "tab", "prev") => Ok(Self::PreviousTab),
            (2, "find", "next") => Ok(Self::FindNext),
            (2, "find", text) if !text.is_empty() => Ok(Self::Find(text.into())),
            (3, "find", "--") if !word(2).is_empty() => Ok(Self::Find(word(2).into())),
            (2, "terminal", "toggle") => Ok(Self::ToggleTerminal),
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
    let mut stream =
        Stream::connect(socket_name(installation).map_err(|error| crate::io_message(&error))?)
            .map_err(|error| {
                format!(
                    "Could not connect to the running Synth: {}",
                    crate::io_message(&error)
                )
            })?;
    exchange(&mut stream, command).map_err(|error| crate::io_message(&error))?
}

fn exchange(stream: &mut Stream, command: &Command) -> io::Result<Reply> {
    #[cfg(unix)]
    check_peer(stream)?;
    write_frame(stream, command)?;
    read_frame(stream, COMMAND_TIMEOUT + Duration::from_secs(1))
}

pub fn listen(installation: &Path, ctx: egui::Context) -> io::Result<mpsc::Receiver<Request>> {
    let name = socket_name(installation)?;
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
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("synth-command".into())
        .spawn(move || {
            while let Ok(mut stream) = listener.accept() {
                let _ = handle_request(&mut stream, &sender, &ctx);
            }
        })?;
    Ok(receiver)
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
        let requests = listen(&installation, egui::Context::default()).unwrap();
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
        assert_eq!(
            dispatch(
                &installation,
                &Command::Open(directory.path().join("caf\u{e9} file.txt").into_os_string())
            ),
            Ok("Opened".into())
        );
        assert_eq!(
            dispatch(&installation, &Command::Close { discard: true }),
            Err("Expected error reply".into())
        );
        server.join().unwrap();
    }
}
