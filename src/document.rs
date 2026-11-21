use std::{
    fs,
    io::{self, Write},
    ops::Range,
    path::{Path, PathBuf},
};

use crate::command::Reply;

pub struct Document {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub text: String,
    pub cursor: usize,
    pub selection: Option<Range<usize>>,
    saved: String,
    bom: bool,
    search: Option<(String, usize)>,
}

impl Document {
    fn empty(id: u64) -> Self {
        Self {
            id,
            path: None,
            text: String::new(),
            cursor: 0,
            selection: None,
            saved: String::new(),
            bom: false,
            search: None,
        }
    }

    pub fn dirty(&self) -> bool {
        self.text != self.saved
    }

    pub fn name(&self) -> String {
        self.path.as_deref().and_then(Path::file_name).map_or_else(
            || "Untitled".into(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    fn display_path(&self) -> String {
        self.path
            .as_ref()
            .map_or_else(|| self.name(), |path| path.display().to_string())
    }

    pub fn position(&self) -> (usize, usize) {
        let mut line = 1;
        let mut column = 1;
        for character in self.text.chars().take(self.cursor) {
            if character == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        (line, column)
    }

    fn properties(&self) -> String {
        let (line, column) = self.position();
        let lines = self.text.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let encoding = if self.bom { "UTF-8 with BOM" } else { "UTF-8" };
        let modified = if self.dirty() { "yes" } else { "no" };
        format!(
            "{}\nModified: {modified}\nEncoding: {encoding}\nLines: {lines}\nCursor: {line}:{column}",
            self.display_path()
        )
    }

    fn save(&mut self, destination: Option<&Path>) -> Reply {
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
        let cursor = self.cursor;
        self.cursor = start;
        let (line, column) = self.position();
        self.cursor = cursor;
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
            if self.current().id != 0 || self.current().path.is_some() || self.current().dirty() {
                self.create();
            }
            let document = self.current_mut();
            document.text = text;
            document.saved.clone_from(&document.text);
            document.bom = bom;
            document.path = Some(path.clone());
        }
        Ok(format!("Opened {}", path.display()))
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
    fn save_preserves_bom_and_refuses_to_overwrite_external_changes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("original.txt");
        let copy = directory.path().join("copy.txt");
        fs::write(&source, "\u{feff}caf\u{e9}\r\n").unwrap();

        let mut documents = Documents::default();
        documents.open(&source).unwrap();
        documents.current_mut().text.push_str("hello");
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
        assert_eq!(document.find(Some("猫".into())).unwrap(), "Match at 1:2");
        assert_eq!(document.selection, Some(1..2));
        assert_eq!(document.find(None).unwrap(), "Match at 2:1");
        assert_eq!(document.selection, Some(3..4));
        assert_eq!(document.find(None).unwrap(), "Match at 1:2");
    }
}
