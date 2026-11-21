use std::{
    collections::hash_map::DefaultHasher,
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

pub fn toggle(installation: &Path) -> io::Result<()> {
    let mut stream = Stream::connect(socket_name(installation)?)?;
    stream.set_recv_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"T")?;
    let mut reply = [0];
    stream.read_exact(&mut reply)?;
    if reply == *b"T" {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid Synth reply",
        ))
    }
}

pub fn listen(installation: &Path, ctx: egui::Context) -> io::Result<mpsc::Receiver<()>> {
    let name = socket_name(installation)?;
    let listener = match ListenerOptions::new().name(name.clone()).create_sync() {
        Err(error)
            if !GenericNamespaced::is_supported()
                && error.kind() == io::ErrorKind::AddrInUse
                && Stream::connect(name.clone())
                    .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionRefused) =>
        {
            ListenerOptions::new()
                .name(name)
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
                let mut request = [0];
                if stream
                    .set_recv_timeout(Some(Duration::from_millis(250)))
                    .is_err()
                    || stream.read_exact(&mut request).is_err()
                    || request != *b"T"
                {
                    continue;
                }
                if sender.send(()).is_err() {
                    break;
                }
                ctx.request_repaint();
                let _ = stream.write_all(b"T");
            }
        })?;
    Ok(receiver)
}
