use std::{
    ffi::OsString,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, Write},
    ops::Range,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::command::Reply;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskChange {
    Modified(u64),
    Unavailable,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Draft {
    path: Option<OsString>,
    text: String,
    saved: String,
    bom: bool,
    cursor: usize,
}

impl Draft {
    pub fn label(&self) -> String {
        self.path.as_ref().map_or_else(|| {
            let preview: String = self.text.lines().map(str::trim).find(|line| !line.is_empty())
                .unwrap_or("").chars().take(60).collect();
            format!("Untitled — {preview}")
        }, |path| Path::new(path).display().to_string())
    }
}

pub struct Document {
    pub id: u64,
    pub revision: u64,
    pub wrap_lines: Option<bool>,
    pub path: Option<PathBuf>,
    pub text: String,
    pub cursor: usize,
    pub selection: Option<Range<usize>>,
    saved: String,
    pub dismissed_disk_change: Option<DiskChange>,
    dirty: bool,
    bom: bool,
    search: Option<(String, usize)>,
    line_starts: Vec<usize>,
    line_ends: Vec<usize>,
    char_count: usize,
}

impl Document {
    fn empty(id: u64) -> Self {
        Self {
            id,
            revision: 0,
            wrap_lines: None,
            path: None,
            text: String::new(),
            cursor: 0,
            selection: None,
            saved: String::new(),
            dismissed_disk_change: None,
            dirty: false,
            bom: false,
            search: None,
            line_starts: vec![0],
            line_ends: vec![0],
            char_count: 0,
        }
    }

    pub fn draft(&self) -> Draft {
        Draft {
            path: self.path.as_ref().map(|path| path.as_os_str().to_owned()),
            text: self.text.clone(),
            saved: self.saved.clone(),
            bom: self.bom,
            cursor: self.cursor,
        }
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn name(&self) -> String {
        self.path.as_deref().and_then(Path::file_name).map_or_else(
            || format!("Untitled {}", self.id + 1),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    pub fn display_path(&self) -> String {
        self.path
            .as_ref()
            .map_or_else(|| self.name(), |path| path.display().to_string())
    }

    pub fn text_changed(&mut self) {
        self.revision += 1;
        self.dirty = self.text != self.saved;
        self.line_starts.clear();
        self.line_starts.push(0);
        self.line_ends.clear();
        let mut carriage_return = false;
        self.char_count = 0;
        for character in self.text.chars() {
            self.char_count += 1;
            if character == '\n' {
                self.line_ends.push(self.char_count - 1 - usize::from(carriage_return));
                self.line_starts.push(self.char_count);
            }
            carriage_return = character == '\r';
        }
        self.line_ends.push(self.char_count);
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn position(&self) -> (usize, usize) {
        self.position_at(self.cursor)
    }

    fn position_at(&self, cursor: usize) -> (usize, usize) {
        let line = self.line_starts.partition_point(|&start| start <= cursor);
        (line, cursor - self.line_starts[line - 1] + 1)
    }

    fn move_cursor(&mut self, position: usize) {
        self.cursor = position;
        self.selection = Some(position..position);
    }

    pub fn goto(&mut self, line: usize, column: usize) -> Reply {
        let start = *self
            .line_starts
            .get(line.wrapping_sub(1))
            .ok_or_else(|| format!("Line {line} is outside this document"))?;
        let end = self.line_ends[line - 1];
        self.move_cursor(start + column.saturating_sub(1).min(end - start));
        let (_, column) = self.position();
        Ok(format!("Cursor at {line}:{column}"))
    }

    pub fn encoding(&self) -> &'static str {
        if self.bom { "UTF-8 with BOM" } else { "UTF-8" }
    }

    fn properties(&self) -> String {
        let (line, column) = self.position();
        let lines = self.line_count();
        let encoding = self.encoding();
        let modified = if self.dirty() { "yes" } else { "no" };
        format!(
            "{}\nModified: {modified}\nEncoding: {encoding}\nLines: {lines}\nCursor: {line}:{column}",
            self.display_path()
        )
    }

    pub fn disk_change(&self) -> Option<DiskChange> {
        let path = self.path.as_ref()?;
        if !path.is_file() {
            return Some(DiskChange::Unavailable);
        }
        let Ok((text, bom)) = read_file(path) else {
            return Some(DiskChange::Unavailable);
        };
        if text == self.saved && bom == self.bom {
            return None;
        }
        let mut hash = DefaultHasher::new();
        text.hash(&mut hash);
        bom.hash(&mut hash);
        Some(DiskChange::Modified(hash.finish()))
    }

    pub fn save(&mut self, destination: Option<&Path>) -> Reply {
        let target = match destination {
            Some(destination) => {
                let name = destination.file_name().ok_or("Expected a file path")?;
                let directory = destination.parent().ok_or("Expected a parent directory")?;
                fs::canonicalize(directory)
                    .map_err(|error| file_error(directory, error))?
                    .join(name)
            }
            None => self
                .path
                .clone()
                .ok_or("Use synth saveas PATH to name this document")?,
        };

        let replace = self.path.as_ref() == Some(&target);
        let parent = target.parent().ok_or("Expected a parent directory")?;
        let mut output =
            tempfile::NamedTempFile::new_in(parent).map_err(|error| file_error(&target, error))?;
        if self.bom {
            output
                .write_all(b"\xef\xbb\xbf")
                .map_err(|error| file_error(&target, error))?;
        }
        output
            .write_all(self.text.as_bytes())
            .map_err(|error| file_error(&target, error))?;
        if replace {
            let (disk, bom) = read_file(&target)?;
            if disk != self.saved || bom != self.bom {
                return Err(format!(
                    "{} changed on disk. Use synth saveas PATH to save a copy.",
                    target.display()
                ));
            }
            let permissions = fs::metadata(&target)
                .map_err(|error| file_error(&target, error))?
                .permissions();
            output
                .as_file()
                .set_permissions(permissions)
                .map_err(|error| file_error(&target, error))?;
            output.persist(&target)
        } else {
            output.persist_noclobber(&target)
        }
        .map_err(|error| file_error(&target, error.error))?;

        self.saved.clone_from(&self.text);
        self.dismissed_disk_change = None;
        self.dirty = false;
        let message = format!("Saved {}", target.display());
        self.path = Some(target);
        Ok(message)
    }

    pub fn find(&mut self, query: Option<String>) -> Reply {
        let (query, start) = match query {
            Some(query) => (query, 0),
            None => self.search.take().ok_or("Run synth find TEXT first")?,
        };
        if query.is_empty() {
            return Err("Search text is empty".into());
        }
        let start_byte = self
            .text
            .char_indices()
            .nth(start)
            .map_or(self.text.len(), |(byte, _)| byte);
        let found = self.text[start_byte..]
            .find(&query)
            .map(|offset| start_byte + offset)
            .or_else(|| self.text.find(&query));
        let Some(byte) = found else {
            self.search = Some((query, 0));
            return Err("No match found".into());
        };
        let start = self.text[..byte].chars().count();
        self.cursor = start + query.chars().count();
        self.selection = Some(start..self.cursor);
        self.search = Some((query, self.cursor));
        let (line, column) = self.position_at(start);
        Ok(format!("Match at {line}:{column}"))
    }
}

fn file_error(path: &Path, error: io::Error) -> String {
    format!("{}: {}", path.display(), crate::io_message(&error))
}

fn read_file(path: &Path) -> Result<(String, bool), String> {
    let mut text = fs::read_to_string(path).map_err(|error| file_error(path, error))?;
    let bom = text.starts_with('\u{feff}');
    if bom {
        text.drain(..'\u{feff}'.len_utf8());
    }
    Ok((text, bom))
}

pub struct Documents {
    pub items: Vec<Document>,
    pub active: usize,
    next_id: u64,
}

impl Default for Documents {
    fn default() -> Self {
        Self {
            items: vec![Document::empty(0)],
            active: 0,
            next_id: 1,
        }
    }
}

impl Documents {
    pub fn current(&self) -> &Document {
        &self.items[self.active]
    }

    pub fn current_mut(&mut self) -> &mut Document {
        &mut self.items[self.active]
    }

    pub fn create(&mut self) {
        self.items.push(Document::empty(self.next_id));
        self.next_id += 1;
        self.active = self.items.len() - 1;
    }

    pub fn open(&mut self, path: &Path) -> Reply {
        let path = fs::canonicalize(path).map_err(|error| file_error(path, error))?;
        if let Some(index) = self
            .items
            .iter()
            .position(|item| item.path.as_ref() == Some(&path))
        {
            self.active = index;
        } else {
            if !path.is_file() {
                return Err(format!("{} is not a regular file", path.display()));
            }
            let (text, bom) = read_file(&path)?;
            let mut document = Document::empty(self.next_id);
            self.next_id += 1;
            document.text = text;
            document.saved.clone_from(&document.text);
            document.text_changed();
            document.bom = bom;
            document.path = Some(path.clone());
            if self.current().id == 0
                && self.current().path.is_none()
                && !self.current().dirty()
                && self.current().text.is_empty()
            {
                self.items[self.active] = document;
            } else {
                self.items.push(document);
                self.active = self.items.len() - 1;
            }
        }
        Ok(format!("Opened {}", path.display()))
    }

    pub fn restore_draft(&mut self, draft: Draft) {
        let mut document = Document::empty(self.next_id);
        self.next_id += 1;
        document.path = draft.path.map(PathBuf::from);
        document.text = draft.text;
        document.saved = draft.saved;
        document.bom = draft.bom;
        document.text_changed();
        document.move_cursor(draft.cursor.min(document.char_count));
        if let Some(index) = self.items.iter().position(|open| {
            !open.dirty()
                && (open.id == 0 || (document.path.is_some() && open.path == document.path))
        }) {
            self.items[index] = document;
            self.active = index;
        } else {
            self.items.push(document);
            self.active = self.items.len() - 1;
        }
    }

    pub fn reload(&mut self, id: u64) -> Reply {
        let document = self
            .items
            .iter_mut()
            .find(|document| document.id == id)
            .ok_or("Document is no longer open")?;
        let path = document
            .path
            .as_ref()
            .ok_or("This document has no file to reload")?;
        if !path.is_file() {
            return Err(format!(
                "{} is unavailable or is not a regular file",
                path.display()
            ));
        }
        let (text, bom) = read_file(path)?;
        let reply = format!("Reloaded {}", path.display());
        let cursor = document.cursor;
        document.text = text;
        document.saved.clone_from(&document.text);
        document.text_changed();
        document.bom = bom;
        document.search = None;
        document.dismissed_disk_change = None;
        document.move_cursor(cursor.min(document.char_count));
        document.id = self.next_id;
        self.next_id += 1;
        Ok(reply)
    }

    pub fn save(&mut self, path: Option<&Path>) -> Reply {
        self.current_mut().save(path)
    }

    pub fn close(&mut self, discard: bool) -> Reply {
        if self.current().dirty() && !discard {
            return Err(
                "Document has unsaved changes. Save it or use synth close --discard.".into(),
            );
        }
        let closed = self.items.remove(self.active).name();
        if self.items.is_empty() {
            self.create();
        } else {
            self.active = self.active.min(self.items.len() - 1);
        }
        Ok(format!("Closed {closed}"))
    }

    pub fn properties(&self) -> String {
        self.current().properties()
    }

    pub fn list(&self) -> String {
        self.items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let active = if index == self.active { ">" } else { " " };
                let modified = if item.dirty() { "*" } else { " " };
                format!("{active}{modified} {}", item.display_path())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn switch(&mut self, next: bool) -> String {
        let count = self.items.len();
        self.active = if next {
            (self.active + 1) % count
        } else {
            (self.active + count - 1) % count
        };
        format!("Active: {}", self.current().display_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goto_clamps_before_crlf_when_inserting_and_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("example.txt");
        fs::write(&path, "猫bc\r\nnext").unwrap();
        let mut docs = Documents::default();
        docs.open(&path).unwrap();
        let doc = docs.current_mut();
        assert_eq!(doc.goto(1, 999).unwrap(), "Cursor at 1:4");
        let byte = doc.text.char_indices().nth(doc.cursor).unwrap().0;
        doc.text.insert(byte, 'x');
        doc.text_changed();
        docs.save(None).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "猫bcx\r\nnext");
    }

    #[test]
    fn disk_changes_compare_against_saved_content_with_bom() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original.txt");
        fs::write(&path, "initial").unwrap();
        let mut documents = Documents::default();
        documents.open(&path).unwrap();
        let document = documents.current_mut();
        document.text.push_str(" edit");
        document.text_changed();
        assert_eq!(document.disk_change(), None);

        fs::write(&path, "\u{feff}initial").unwrap();
        let bom_change = document.disk_change();
        assert!(matches!(bom_change, Some(DiskChange::Modified(_))));
        fs::write(&path, "initial edit").unwrap();
        assert_ne!(document.disk_change(), bom_change);
        fs::remove_file(&path).unwrap();
        assert_eq!(document.disk_change(), Some(DiskChange::Unavailable));
    }

    #[test]
    fn save_preserves_bom_and_refuses_to_overwrite_external_changes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("original.txt");
        let copy = directory.path().join("copy.txt");
        fs::write(&source, "\u{feff}caf\u{e9}\r\n").unwrap();

        let mut documents = Documents::default();
        documents.open(&source).unwrap();
        documents.current_mut().text.push_str("hello");
        documents.current_mut().text_changed();
        assert!(documents.close(false).is_err());
        documents.save(None).unwrap();
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "\u{feff}caf\u{e9}\r\nhello"
        );

        fs::write(&source, "External").unwrap();
        assert!(documents.save(None).is_err());
        documents.save(Some(&copy)).unwrap();
        assert_eq!(
            fs::read_to_string(&copy).unwrap(),
            "\u{feff}caf\u{e9}\r\nhello"
        );
        assert!(documents.save(Some(&source)).is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), "External");
    }

    #[test]
    fn find_next_selects_unicode_character_ranges_and_wraps() {
        let mut documents = Documents::default();
        let document = documents.current_mut();
        document.text = "é猫\n猫".into();
        document.text_changed();
        assert_eq!(document.find(Some("猫".into())).unwrap(), "Match at 1:2");
        assert_eq!(document.selection, Some(1..2));
        assert_eq!(document.find(None).unwrap(), "Match at 2:1");
        assert_eq!(document.selection, Some(3..4));
        assert_eq!(document.find(None).unwrap(), "Match at 1:2");
    }

    #[test]
    fn goto_and_reload_preserve_character_positions_and_reset_editor_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, "éé\n猫猫").unwrap();
        let mut documents = Documents::default();
        documents.open(&path).unwrap();
        let document = documents.current_mut();
        let id = document.id;
        assert_eq!(document.goto(2, 2).unwrap(), "Cursor at 2:2");
        assert_eq!(document.selection, Some(4..4));
        assert!(document.goto(3, 1).is_err());
        fs::write(&path, "\u{feff}猫").unwrap();
        documents.reload(id).unwrap();
        let document = documents.current();
        assert_ne!(document.id, id);
        assert_eq!(document.text, "猫");
        assert_eq!(document.selection, Some(1..1));
        assert_eq!(document.position(), (1, 2));
        assert!(document.bom);
        assert!(!document.dirty());
    }

    #[test]
    fn opening_file_uses_fresh_editor_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("example.txt");
        fs::write(&path, "Keep this content").unwrap();
        let mut documents = Documents::default();
        let initial_id = documents.current().id;
        documents.open(&path).unwrap();
        assert_ne!(documents.current().id, initial_id);
        assert_eq!(documents.current().text, "Keep this content");
        assert!(!documents.current().dirty());
        assert_eq!(documents.items.len(), 1);

        let opened_id = documents.current().id;
        documents.open(&path).unwrap();
        assert_eq!(documents.current().id, opened_id);
        documents.close(false).unwrap();
        documents.open(&path).unwrap();
        assert_ne!(documents.current().id, opened_id);
        assert_eq!(documents.current().text, "Keep this content");
    }

    #[test]
    fn opening_file_preserves_existing_empty_untitled_tab() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("document.txt");
        fs::write(&path, "Hello").unwrap();
        let mut documents = Documents::default();
        documents.create();
        let untitled_id = documents.current().id;
        documents.open(&path).unwrap();
        assert_eq!(documents.items.len(), 3);
        assert_eq!(documents.items[1].id, untitled_id);
        assert!(documents.items[1].path.is_none());
        assert_eq!(documents.current().text, "Hello");
    }

}
