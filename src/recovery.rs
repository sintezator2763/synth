use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::document::{Documents, Draft};

pub struct Recovery {
    _lock: File,
    directory: tempfile::TempDir,
    recorded: HashMap<u64, u64>,
    writer: Option<JoinHandle<io::Result<HashMap<u64, u64>>>>,
    next_write: Instant,
    previous: Vec<PreviousSession>,
    pub warnings: Vec<String>,
}

struct PreviousSession {
    _lock: File,
    files: Vec<Snapshot>,
}

struct Snapshot {
    path: PathBuf,
    rows: Vec<Row>,
}

struct Row {
    draft: Draft,
    selected: bool,
    restored: bool,
    label: String,
}

fn write_snapshot(path: &Path, drafts: &[&Draft]) -> io::Result<()> {
    if drafts.is_empty() {
        return match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }
    let mut output = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    let mut writer = BufWriter::new(output.as_file_mut());
    serde_json::to_writer(&mut writer, drafts)?;
    writer.flush()?;
    drop(writer);
    output.as_file().sync_all()?;
    output.persist(path).map_err(|error| error.error)?;
    Ok(())
}

impl Recovery {
    pub fn new(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let directory = tempfile::Builder::new()
            .prefix("session-")
            .tempdir_in(root)?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("lock"))?;
        lock.lock()?;
        let mut recovery = Self {
            _lock: lock,
            directory,
            recorded: HashMap::new(),
            writer: None,
            next_write: Instant::now(),
            previous: Vec::new(),
            warnings: Vec::new(),
        };
        for entry in fs::read_dir(root)? {
            let result = entry.and_then(|entry| {
                if entry.file_type()?.is_dir()
                    && entry.file_name().to_string_lossy().starts_with("session-")
                    && entry.path() != recovery.directory.path()
                {
                    recovery.load_session(&entry.path())?;
                }
                Ok(())
            });
            if let Err(error) = result {
                recovery.warnings.push(error.to_string());
            }
        }
        Ok(recovery)
    }

    fn load_session(&mut self, path: &Path) -> io::Result<()> {
        let lock = File::options()
            .read(true)
            .write(true)
            .open(path.join("lock"))?;
        if lock.try_lock().is_err() {
            return Ok(());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(path)? {
            let file = entry?.path();
            if file.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let result: io::Result<Vec<Draft>> = File::open(&file)
                .and_then(|stream| Ok(serde_json::from_reader(BufReader::new(stream))?));
            match result {
                Ok(drafts) => files.push(Snapshot {
                    path: file,
                    rows: drafts
                        .into_iter()
                        .map(|draft| Row {
                            label: draft.label(),
                            draft,
                            selected: true,
                            restored: false,
                        })
                        .collect(),
                }),
                Err(error) => self.warnings.push(format!("{}: {error}", file.display())),
            }
        }
        if !files.is_empty() {
            self.previous.push(PreviousSession { _lock: lock, files });
        }
        Ok(())
    }

    pub fn choices(&mut self) -> impl Iterator<Item = (&str, &mut bool)> {
        self.previous
            .iter_mut()
            .flat_map(|session| &mut session.files)
            .flat_map(|file| &mut file.rows)
            .filter(|row| !row.restored)
            .map(|row| (row.label.as_str(), &mut row.selected))
    }

    pub fn restore(&mut self, documents: &mut Documents) {
        for row in self
            .previous
            .iter_mut()
            .flat_map(|s| &mut s.files)
            .flat_map(|f| &mut f.rows)
        {
            if row.selected && !row.restored {
                documents.restore_draft(row.draft.clone());
                row.restored = true;
                row.selected = false;
            }
        }
    }

    fn remove_rows(&mut self, remove: impl Fn(&Row) -> bool) -> io::Result<()> {
        for session in &mut self.previous {
            for file in &mut session.files {
                if file.rows.iter().any(&remove) {
                    let keep = file
                        .rows
                        .iter()
                        .filter(|row| !remove(row))
                        .map(|r| &r.draft)
                        .collect::<Vec<_>>();
                    write_snapshot(&file.path, &keep)?;
                    file.rows.retain(|row| !remove(row));
                }
            }
            session.files.retain(|file| !file.rows.is_empty());
        }
        self.previous.retain(|session| !session.files.is_empty());
        Ok(())
    }

    pub fn discard(&mut self) -> io::Result<()> {
        self.remove_rows(|row| row.selected && !row.restored)
    }

    pub fn sync(&mut self, ctx: &eframe::egui::Context, documents: &Documents) -> io::Result<()> {
        if let Some(writer) = &self.writer {
            if !writer.is_finished() {
                ctx.request_repaint_after(Duration::from_millis(50));
                return Ok(());
            }
            self.recorded = self
                .writer
                .take()
                .unwrap()
                .join()
                .map_err(|_| io::Error::other("Draft writer stopped"))??;
        }
        let revisions: HashMap<_, _> = documents
            .items
            .iter()
            .filter(|doc| doc.dirty())
            .map(|doc| (doc.id, doc.revision))
            .collect();
        if revisions == self.recorded {
            return self.remove_rows(|row| row.restored);
        }
        let now = Instant::now();
        if self.next_write > now
            && revisions.keys().all(|id| self.recorded.contains_key(id))
            && self.recorded.len() == revisions.len()
        {
            ctx.request_repaint_after(self.next_write - now);
            return Ok(());
        }
        let root = self.directory.path().to_owned();
        let changed: Vec<_> = documents
            .items
            .iter()
            .filter(|doc| doc.dirty() && self.recorded.get(&doc.id) != Some(&doc.revision))
            .map(|doc| (doc.id, doc.draft()))
            .collect();
        let removed: Vec<_> = self
            .recorded
            .keys()
            .filter(|id| !revisions.contains_key(id))
            .copied()
            .collect();
        self.next_write = now + Duration::from_millis(500);
        self.writer = Some(
            std::thread::Builder::new()
                .name("synth-drafts".into())
                .spawn(move || {
                    for (id, draft) in changed {
                        write_snapshot(&root.join(format!("draft-{id}.json")), &[&draft])?;
                    }
                    for id in removed {
                        write_snapshot(&root.join(format!("draft-{id}.json")), &[])?;
                    }
                    Ok(revisions)
                })?,
        );
        ctx.request_repaint_after(Duration::from_millis(50));
        Ok(())
    }
}

impl Drop for Recovery {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish(recovery: &mut Recovery, documents: &Documents) {
        let ctx = eframe::egui::Context::default();
        recovery.next_write = Instant::now();
        recovery.sync(&ctx, documents).unwrap();
        if let Some(handle) = recovery.writer.take() {
            recovery.recorded = handle.join().unwrap().unwrap();
        }
        recovery.sync(&ctx, documents).unwrap();
    }

    #[test]
    fn damaged_snapshot_does_not_block_selective_recovery_or_new_writes() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("session-legacy");
        fs::create_dir(&old).unwrap();
        File::create(old.join("lock")).unwrap();
        fs::write(old.join("broken.json"), "{").unwrap();
        let mut docs = Documents::default();
        docs.current_mut().text = "keep this".into();
        docs.current_mut().text_changed();
        let first = docs.current().draft();
        docs.current_mut().text = "recover this".into();
        docs.current_mut().text_changed();
        let second = docs.current().draft();
        write_snapshot(&old.join("drafts.json"), &[&first, &second]).unwrap();
        let mut recovery = Recovery::new(root.path()).unwrap();
        assert!(recovery.warnings[0].contains("broken.json"));
        for (label, selected) in recovery.choices() {
            *selected = label.contains("recover this");
        }
        let mut restored = Documents::default();
        recovery.restore(&mut restored);
        finish(&mut recovery, &restored);
        assert_eq!(restored.current().text, "recover this");
        let snapshot = recovery
            .directory
            .path()
            .join(format!("draft-{}.json", restored.current().id));
        assert!(snapshot.is_file());
        let old_drafts: Vec<Draft> =
            serde_json::from_reader(File::open(old.join("drafts.json")).unwrap()).unwrap();
        assert_eq!(old_drafts.len(), 1);
        assert!(old_drafts[0].label().contains("keep this"));
        assert_eq!(fs::read_to_string(old.join("broken.json")).unwrap(), "{");
        *recovery.choices().next().unwrap().1 = true;
        recovery.discard().unwrap();
        assert!(!old.join("drafts.json").exists());
        assert!(snapshot.is_file());
    }
}
