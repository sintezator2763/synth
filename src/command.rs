use std::{
    collections::hash_map::DefaultHasher,
    ffi::OsString,
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
    ToggleFocus,
    ConfigPath,
    ConfigCheck,
    ConfigReload,
}

pub const USAGE: &str = "Usage: synth [open PATH | new | save | saveas PATH | close [-d|--discard] | properties | tab list|next|prev | find TEXT|next | terminal toggle | focus toggle | config path|check|reload]";

impl Command {
    pub fn parse(args: &[OsString], directory: &Path) -> Result<Self, String> {
        let word = |index: usize| args.get(index).and_then(|arg| arg.to_str()).unwrap_or("");
        match (args.len(), word(0), word(1)) {
            (2, "open", _) => Ok(Self::Open(directory.join(&args[1]).into_os_string())),
            (1, "new", _) => Ok(Self::New),
            (1, "save", _) => Ok(Self::Save),
            (2, "saveas", _) => Ok(Self::SaveAs(directory.join(&args[1]).into_os_string())),
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
            (2, "focus", "toggle") => Ok(Self::ToggleFocus),
            (2, "config", "path") => Ok(Self::ConfigPath),
            (2, "config", "check") => Ok(Self::ConfigCheck),
            (2, "config", "reload") => Ok(Self::ConfigReload),
            _ => Err(USAGE.into()),
        }
    }
}

pub type Reply = Result<String, String>;

pub struct Request {
    pub command: Command,
    pub respond: mpsc::SyncSender<Reply>,
}

const MAX_FRAME: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

fn receive<T: DeserializeOwned>(stream: &mut Stream) -> io::Result<T> {
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

fn send<T: Serialize>(stream: &mut Stream, value: &T) -> io::Result<()> {
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
    let mut stream = Stream::connect(socket_name(installation).map_err(|error| error.to_string())?)
        .map_err(|error| format!("Could not connect to the running Synth: {error}"))?;
    exchange(&mut stream, command).map_err(|error| error.to_string())?
}

fn exchange(stream: &mut Stream, command: &Command) -> io::Result<Reply> {
    #[cfg(unix)]
    check_peer(stream)?;
    stream.set_recv_timeout(Some(COMMAND_TIMEOUT + Duration::from_secs(1)))?;
    stream.set_send_timeout(Some(Duration::from_secs(1)))?;
    send(stream, command)?;
    receive(stream)
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
    stream.set_recv_timeout(Some(Duration::from_millis(500)))?;
    stream.set_send_timeout(Some(Duration::from_secs(1)))?;
    let command = receive(stream)?;
    let (respond, response) = mpsc::sync_channel(1);
    sender
        .send(Request { command, respond })
        .map_err(io::Error::other)?;
    ctx.request_repaint();
    let reply = response
        .recv_timeout(COMMAND_TIMEOUT)
        .unwrap_or_else(|_| Err("Synth did not complete the command in time".into()));
    send(stream, &reply)
}
