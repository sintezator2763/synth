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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Utf16LeRaw,
    Utf16BeRaw,
}

impl Encoding {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "utf8" | "utf-8" => Ok(Self::Utf8),
            "utf8-bom" | "utf-8-bom" => Ok(Self::Utf8Bom),
            "utf16le" | "utf-16le" => Ok(Self::Utf16Le),
            "utf16be" | "utf-16be" => Ok(Self::Utf16Be),
            _ => Err("Expected utf8, utf8-bom, utf16le, or utf16be".into()),
        }
    }

    fn accepts(self, detected: Self) -> bool {
        self == detected
            || matches!(
                (self, detected),
                (Self::Utf16Le, Self::Utf16LeRaw) | (Self::Utf16Be, Self::Utf16BeRaw)
            )
    }

    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 with BOM",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Utf16LeRaw => "UTF-16 LE (no BOM)",
            Self::Utf16BeRaw => "UTF-16 BE (no BOM)",
        }
    }

    fn write(self, text: &str, output: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Utf8 => output.write_all(text.as_bytes()),
            Self::Utf8Bom => {
                output.write_all(b"\xef\xbb\xbf")?;
                output.write_all(text.as_bytes())
            }
            Self::Utf16Le | Self::Utf16Be | Self::Utf16LeRaw | Self::Utf16BeRaw => {
                let little = matches!(self, Self::Utf16Le | Self::Utf16LeRaw);
                if matches!(self, Self::Utf16Le | Self::Utf16Be) {
                    output.write_all(if little { b"\xff\xfe" } else { b"\xfe\xff" })?;
                }
                let mut output = io::BufWriter::new(output);
                for unit in text.encode_utf16() {
                    output.write_all(&if little {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    })?;
                }
                output.flush()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "lf" => Ok(Self::Lf),
            "crlf" => Ok(Self::Crlf),
            _ => Err("Expected lf or crlf".into()),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
        }
    }

    pub fn separator(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }

    pub fn detect(text: &str) -> Self {
        match text.find('\n') {
            Some(index) if text[..index].ends_with('\r') => Self::Crlf,
            _ => Self::Lf,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Draft {
    path: Option<OsString>,
    text: String,
    saved: String,
    #[serde(default)]
    bom: bool,
    #[serde(default)]
    encoding: Option<Encoding>,
    #[serde(default)]
    saved_encoding: Option<Encoding>,
    #[serde(default)]
    ending: Option<LineEnding>,
    cursor: usize,
}

impl Draft {
    pub fn label(&self) -> String {
        self.path.as_ref().map_or_else(
            || {
                let preview: String = self
                    .text
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect();
                format!("Untitled — {preview}")
            },
            |path| Path::new(path).display().to_string(),
        )
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
    encoding: Encoding,
    saved_encoding: Encoding,
    pub ending: LineEnding,
    pub selected_range: Option<Range<usize>>,
    search: Option<(String, usize)>,
    line_starts: Vec<usize>,
    byte_starts: Vec<usize>,
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
            encoding: Encoding::Utf8,
            saved_encoding: Encoding::Utf8,
            ending: LineEnding::Lf,
            selected_range: None,
            search: None,
            line_starts: vec![0],
            byte_starts: vec![0],
            line_ends: vec![0],
            char_count: 0,
        }
    }

    pub fn draft(&self) -> Draft {
        Draft {
            path: self.path.as_ref().map(|path| path.as_os_str().to_owned()),
            text: self.text.clone(),
            saved: self.saved.clone(),
            bom: self.encoding == Encoding::Utf8Bom,
            encoding: Some(self.encoding),
            saved_encoding: Some(self.saved_encoding),
            ending: Some(self.ending),
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

    fn mark_changed(&mut self) {
        self.revision += 1;
        self.dirty = self.encoding != self.saved_encoding || self.text != self.saved;
    }

    pub fn text_changed(&mut self) {
        self.reindex_from(0);
        self.mark_changed();
    }

    fn reindex_from(&mut self, line: usize) {
        let mut byte = self.byte_starts[line];
        self.char_count = self.line_starts[line];
        self.line_starts.truncate(line + 1);
        self.byte_starts.truncate(line + 1);
        self.line_ends.truncate(line);
        for row in self.text[byte..].split_inclusive('\n') {
            self.char_count += row.chars().count();
            byte += row.len();
            if row.ends_with('\n') {
                self.line_ends
                    .push(self.char_count - 1 - usize::from(row.ends_with("\r\n")));
                self.line_starts.push(self.char_count);
                self.byte_starts.push(byte);
            }
        }
        self.line_ends.push(self.char_count);
    }

    pub fn replace_chars(&mut self, range: Range<usize>, replacement: &str) -> usize {
        let start = range.start.min(self.char_count);
        let end = range.end.min(self.char_count).max(start);
        let bytes = self.byte_at_char(start)..self.byte_at_char(end);
        let count = replacement.chars().count();
        if self.text[bytes.clone()] == *replacement {
            return count;
        }
        let line = self.position_at(start).0 - 1;
        let last = self.position_at(end).0 - 1;
        if line == last && !replacement.contains('\n') {
            let delta_chars = count as isize - (end - start) as isize;
            let delta_bytes = replacement.len() as isize - bytes.len() as isize;
            self.text.replace_range(bytes, replacement);
            self.char_count = self.char_count.saturating_add_signed(delta_chars);
            for next in line + 1..self.line_starts.len() {
                self.line_starts[next] = self.line_starts[next].saturating_add_signed(delta_chars);
                self.byte_starts[next] = self.byte_starts[next].saturating_add_signed(delta_bytes);
                self.line_ends[next] = self.line_ends[next].saturating_add_signed(delta_chars);
            }
            self.line_ends[line] = self
                .line_starts
                .get(line + 1)
                .map_or(self.char_count, |next| {
                    let byte = self.byte_starts[line + 1];
                    next - 1 - usize::from(self.text[..byte].ends_with("\r\n"))
                });
        } else if let Some(&suffix) = self.byte_starts.get(last + 1) {
            let delta_chars = count as isize - (end - start) as isize;
            let delta_bytes = replacement.len() as isize - bytes.len() as isize;
            self.text.replace_range(bytes, replacement);
            let mut byte = self.byte_starts[line];
            let mut chars = self.line_starts[line];
            let mut starts = Vec::new();
            let mut ends = Vec::new();
            let mut byte_starts = Vec::new();
            let changed_end = suffix.saturating_add_signed(delta_bytes);
            for row in self.text[byte..changed_end].split_inclusive('\n') {
                byte_starts.push(byte);
                starts.push(chars);
                byte += row.len();
                chars += row.chars().count();
                ends.push(chars - 1 - usize::from(row.ends_with("\r\n")));
            }
            for index in last + 1..self.line_starts.len() {
                self.line_starts[index] =
                    self.line_starts[index].saturating_add_signed(delta_chars);
                self.line_ends[index] = self.line_ends[index].saturating_add_signed(delta_chars);
                self.byte_starts[index] =
                    self.byte_starts[index].saturating_add_signed(delta_bytes);
            }
            self.line_starts.splice(line..=last, starts);
            self.line_ends.splice(line..=last, ends);
            self.byte_starts.splice(line..=last, byte_starts);
            self.char_count = self.char_count.saturating_add_signed(delta_chars);
        } else {
            self.text.replace_range(bytes, replacement);
            self.reindex_from(line);
        }
        self.mark_changed();
        count
    }

    pub fn byte_at_char(&self, index: usize) -> usize {
        if index >= self.char_count {
            return self.text.len();
        }
        let line = self.position_at(index).0 - 1;
        let start = self.byte_starts[line];
        self.text[start..]
            .char_indices()
            .nth(index - self.line_starts[line])
            .map_or(self.text.len(), |(byte, _)| start + byte)
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn position(&self) -> (usize, usize) {
        self.position_at(self.cursor)
    }

    pub fn column_at(&self, cursor: usize) -> usize {
        self.position_at(cursor).1
    }

    pub fn clamp_cursor(&self, cursor: usize) -> usize {
        cursor.min(self.char_count)
    }

    fn position_at(&self, cursor: usize) -> (usize, usize) {
        let cursor = self.clamp_cursor(cursor);
        let line = self.line_starts.partition_point(|&start| start <= cursor);
        (
            line,
            cursor.min(self.line_ends[line - 1]) - self.line_starts[line - 1] + 1,
        )
    }

    fn move_cursor(&mut self, position: usize) {
        self.cursor = position;
        self.selection = Some(position..position);
        self.selected_range = None;
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
        self.encoding.label()
    }

    pub fn leading_spaces_at(&self, cursor: usize) -> &str {
        let line = self.position_at(cursor).0 - 1;
        let start = self.byte_starts[line];
        let end = self.byte_at_char(cursor);
        let prefix = &self.text[start..end];
        &prefix[..prefix.len() - prefix.trim_start_matches([' ', '\t']).len()]
    }

    pub fn duplicate_line(&mut self) -> Reply {
        let (line, column) = self.position();
        let index = line - 1;
        let contents =
            &self.text[self.byte_starts[index]..self.byte_at_char(self.line_ends[index])];
        let ending = self.ending.separator();
        let (at, inserted) = if let Some(next) = self.line_starts.get(index + 1) {
            (*next, format!("{contents}{ending}"))
        } else {
            (self.char_count, format!("{ending}{contents}"))
        };
        self.replace_chars(at..at, &inserted);
        self.goto(line + 1, column)
    }

    pub fn move_line(&mut self, down: bool) -> Reply {
        let (line, column) = self.position();
        let first = if down {
            line - 1
        } else {
            line.saturating_sub(2)
        };
        let lines = self.line_count() - usize::from(self.text.ends_with('\n'));
        if (!down && line == 1) || first + 1 >= lines {
            return Err("No line in that direction".into());
        }
        let second = first + 1;
        let start = self.line_starts[first];
        let end = self.line_ends[second];
        let a = &self.text[self.byte_starts[first]..self.byte_at_char(self.line_ends[first])];
        let b = &self.text[self.byte_starts[second]..self.byte_at_char(end)];
        let separator =
            &self.text[self.byte_at_char(self.line_ends[first])..self.byte_starts[second]];
        let replacement = format!("{b}{separator}{a}");
        self.replace_chars(start..end, &replacement);
        self.goto(if down { line + 1 } else { line - 1 }, column)
    }

    pub fn indent(&mut self, increase: bool, spaces: bool, size: usize) -> Reply {
        let selected = self
            .selection
            .as_ref()
            .or(self.selected_range.as_ref())
            .filter(|range| !range.is_empty())
            .cloned();
        let (start, end) = selected
            .as_ref()
            .map_or((self.cursor, self.cursor), |range| {
                (range.start, range.end - 1)
            });
        let (first, column) = self.position_at(start);
        let last = self.position_at(end).0;
        let prefix = if spaces {
            " ".repeat(size)
        } else {
            "\t".to_owned()
        };
        let start = self.line_starts[first - 1];
        let end = self.line_ends[last - 1];
        let first_offset = self.byte_starts[first - 1];
        let range = &self.text[first_offset..self.byte_at_char(end)];
        let mut replacement = String::with_capacity(range.len());
        let mut first_change = 0;
        for (index, row) in range
            .split_inclusive('\n')
            .chain((range.is_empty() || range.ends_with('\n')).then_some(""))
            .enumerate()
        {
            if increase {
                replacement.push_str(&prefix);
                replacement.push_str(row);
            } else {
                let trim = if row.starts_with('\t') {
                    1
                } else {
                    row.bytes()
                        .take(size)
                        .take_while(|&byte| byte == b' ')
                        .count()
                };
                if index == 0 {
                    first_change = trim;
                }
                replacement.push_str(&row[trim..]);
            }
        }
        let count = self.replace_chars(start..end, &replacement);
        if selected.is_some() {
            self.cursor = start + count;
            self.selection = Some(start..self.cursor);
            self.selected_range = self.selection.clone();
        } else {
            let column = if increase {
                column + prefix.len()
            } else {
                column.saturating_sub(first_change).max(1)
            };
            self.goto(first, column)?;
        }
        Ok(format!(
            "{first}–{last}: {}",
            if increase { "indented" } else { "unindented" }
        ))
    }

    pub fn set_encoding(&mut self, encoding: Encoding) -> Reply {
        self.encoding = encoding;
        self.mark_changed();
        Ok(format!("Encoding: {}", self.encoding()))
    }

    pub fn replace_formatted(&mut self, text: String, cursor: (usize, usize)) -> Reply {
        self.text = text;
        self.set_ending_at(self.ending, cursor)
    }

    pub fn set_ending(&mut self, ending: LineEnding) -> Reply {
        self.set_ending_at(ending, self.position())
    }

    fn set_ending_at(&mut self, ending: LineEnding, cursor: (usize, usize)) -> Reply {
        self.ending = ending;
        let mut text = String::with_capacity(self.text.len());
        for row in self.text.split_inclusive('\n') {
            if let Some(content) = row.strip_suffix('\n') {
                text.push_str(content.strip_suffix('\r').unwrap_or(content));
                text.push_str(ending.separator());
            } else {
                text.push_str(row);
            }
        }
        self.text = text;
        self.text_changed();
        self.goto(cursor.0.min(self.line_count()), cursor.1)?;
        Ok(format!("Line endings: {}", ending.label()))
    }

    fn properties(&self) -> String {
        let (line, column) = self.position();
        let lines = self.line_count();
        let encoding = self.encoding();
        let modified = if self.dirty() { "yes" } else { "no" };
        format!(
            "{}\nModified: {modified}\nEncoding: {encoding}\nLine endings: {}\nLines: {lines}\nCursor: {line}:{column}",
            self.display_path(),
            self.ending.label()
        )
    }

    pub fn disk_change(&self) -> Option<DiskChange> {
        let path = self.path.as_ref()?;
        if !path.is_file() {
            return Some(DiskChange::Unavailable);
        }
        let Ok((text, bom)) = read_file(path, Some(self.saved_encoding)) else {
            return Some(DiskChange::Unavailable);
        };
        if text == self.saved && bom == self.saved_encoding {
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
        self.encoding
            .write(&self.text, &mut output)
            .map_err(|error| file_error(&target, error))?;
        if replace {
            let (disk, bom) = read_file(&target, Some(self.saved_encoding))?;
            if disk != self.saved || bom != self.saved_encoding {
                return Err(format!(
                    "{} changed on disk. Use synth saveas PATH to save a copy.",
                    target.display()
                ));
            }
            let permissions = fs::metadata(&target)
                .map_err(|error| file_error(&target, error))?
                .permissions();
            if permissions.readonly() {
                return Err(format!(
                    "{} is read-only. Use synth saveas PATH to save a copy.",
                    target.display()
                ));
            }
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
        self.saved_encoding = self.encoding;
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
        let start_byte = self.byte_at_char(start);
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

fn read_file(path: &Path, preferred: Option<Encoding>) -> Result<(String, Encoding), String> {
    let bytes = fs::read(path).map_err(|error| file_error(path, error))?;
    let invalid = || format!("{}: invalid or unsupported text encoding", path.display());
    if bytes.starts_with(b"\xff\xfe\0\0") || bytes.starts_with(b"\0\0\xfe\xff") {
        return Err(invalid());
    }
    let (encoding, prefix) = if bytes.starts_with(b"\xff\xfe") {
        (Encoding::Utf16Le, 2)
    } else if bytes.starts_with(b"\xfe\xff") {
        (Encoding::Utf16Be, 2)
    } else if bytes.starts_with(b"\xef\xbb\xbf") {
        (Encoding::Utf8Bom, 3)
    } else {
        (
            match preferred {
                Some(Encoding::Utf16Le | Encoding::Utf16LeRaw) => Encoding::Utf16LeRaw,
                Some(Encoding::Utf16Be | Encoding::Utf16BeRaw) => Encoding::Utf16BeRaw,
                _ => Encoding::Utf8,
            },
            0,
        )
    };
    let text = match encoding {
        Encoding::Utf16Le | Encoding::Utf16Be | Encoding::Utf16LeRaw | Encoding::Utf16BeRaw => {
            let (chunks, remainder) = bytes[prefix..].as_chunks::<2>();
            if !remainder.is_empty() {
                return Err(invalid());
            }
            let units = chunks.iter().copied().map(|unit| {
                if matches!(encoding, Encoding::Utf16Le | Encoding::Utf16LeRaw) {
                    u16::from_le_bytes(unit)
                } else {
                    u16::from_be_bytes(unit)
                }
            });
            char::decode_utf16(units)
                .collect::<Result<String, _>>()
                .map_err(|_| invalid())?
        }
        _ => {
            if bytes[prefix..].contains(&0) {
                return Err(invalid());
            }
            let mut text = String::from_utf8(bytes).map_err(|_| invalid())?;
            text.drain(..prefix);
            text
        }
    };
    Ok((text, encoding))
}

pub struct Documents {
    pub items: Vec<Document>,
    pub active: usize,
    next_id: u64,
    closed: Vec<(Option<PathBuf>, Encoding)>,
}

impl Default for Documents {
    fn default() -> Self {
        Self {
            items: vec![Document::empty(0)],
            active: 0,
            next_id: 1,
            closed: Vec::new(),
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
        self.open_encoded(path, None)
    }

    pub fn open_encoded(&mut self, path: &Path, encoding: Option<Encoding>) -> Reply {
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
            let (text, detected) = read_file(&path, encoding)?;
            if let Some(encoding) = encoding
                && !encoding.accepts(detected)
            {
                return Err(format!(
                    "{} uses {}, expected {}",
                    path.display(),
                    detected.label(),
                    encoding.label()
                ));
            }
            let mut document = Document::empty(self.next_id);
            self.next_id += 1;
            document.text = text;
            document.saved.clone_from(&document.text);
            document.ending = LineEnding::detect(&document.text);
            document.encoding = detected;
            document.saved_encoding = detected;
            document.text_changed();
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
        document.encoding = draft.encoding.unwrap_or(if draft.bom {
            Encoding::Utf8Bom
        } else {
            Encoding::Utf8
        });
        document.saved_encoding = draft.saved_encoding.unwrap_or(document.encoding);
        document.ending = draft
            .ending
            .unwrap_or_else(|| LineEnding::detect(&document.text));
        document.text_changed();
        document.move_cursor(draft.cursor.min(document.char_count));
        let existing = self.items.iter().position(|open| {
            !open.dirty() && document.path.is_some() && open.path == document.path
        });
        if let Some(index) = existing.or_else(|| {
            self.items
                .iter()
                .position(|open| open.id == 0 && !open.dirty())
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
        let (text, bom) = read_file(path, Some(document.saved_encoding))?;
        let reply = format!("Reloaded {}", path.display());
        let cursor = document.cursor;
        document.text = text;
        document.saved.clone_from(&document.text);
        document.encoding = bom;
        document.saved_encoding = bom;
        document.ending = LineEnding::detect(&document.text);
        document.text_changed();
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
        let document = self.items.remove(self.active);
        let closed = document.name();
        if document.path.is_some() {
            self.closed
                .retain(|(previous, _)| previous != &document.path);
        }
        self.closed.push((document.path, document.saved_encoding));
        if self.closed.len() > 32 {
            self.closed.remove(0);
        }
        if self.items.is_empty() {
            self.create();
        } else {
            self.active = self.active.min(self.items.len() - 1);
        }
        Ok(format!("Closed {closed}"))
    }

    pub fn reopen(&mut self) -> Reply {
        let (path, encoding) = self
            .closed
            .last()
            .cloned()
            .ok_or("No recently closed document")?;
        let reply = if let Some(path) = path {
            let raw = matches!(encoding, Encoding::Utf16LeRaw | Encoding::Utf16BeRaw);
            self.open_encoded(&path, raw.then_some(encoding))?
        } else {
            self.create();
            "Reopened untitled document".into()
        };
        self.closed.pop();
        Ok(reply)
    }

    pub fn select(&mut self, number: usize) -> Reply {
        if !(1..=self.items.len()).contains(&number) {
            return Err(format!("Tab {number} does not exist"));
        }
        self.active = number - 1;
        Ok(format!("Active: {}", self.current().display_path()))
    }

    pub fn save_all(&mut self) -> Reply {
        let mut saved = 0;
        let mut errors = Vec::new();
        for document in self.items.iter_mut().filter(|doc| doc.dirty()) {
            match document.save(None) {
                Ok(_) => saved += 1,
                Err(error) => errors.push(format!("{}: {error}", document.name())),
            }
        }
        if errors.is_empty() {
            Ok(format!("Saved {saved} documents"))
        } else {
            Err(format!("Saved {saved} documents. {}", errors.join("\n")))
        }
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
    fn incremental_edits_match_a_full_index_with_unicode_and_crlf() {
        let mut document = Document::empty(1);
        document.text = "é猫\r\nline\nおわり".into();
        document.text_changed();
        for (range, replacement) in [
            (0..1, "🦀"),
            (3..5, ""),
            (1..2, "\r\n"),
            (0..0, "α"),
            (5..6, "終"),
        ] {
            document.replace_chars(range, replacement);
            let mut expected = Document::empty(2);
            expected.text.clone_from(&document.text);
            expected.text_changed();
            assert_eq!(document.line_starts, expected.line_starts);
            assert_eq!(document.byte_starts, expected.byte_starts);
            assert_eq!(document.line_ends, expected.line_ends);
            assert_eq!(document.char_count, expected.char_count);
        }
    }

    #[test]
    fn encodings_round_trip_and_conversion_is_dirty_without_a_text_change() {
        let dir = tempfile::tempdir().unwrap();
        let original = "é🦀\r\n猫";
        for encoding in [
            Encoding::Utf8,
            Encoding::Utf8Bom,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Utf16LeRaw,
            Encoding::Utf16BeRaw,
        ] {
            let path = dir.path().join(format!("{encoding:?}.txt"));
            let mut bytes = Vec::new();
            encoding.write(original, &mut bytes).unwrap();
            fs::write(&path, &bytes).unwrap();
            let mut docs = Documents::default();
            let explicit =
                matches!(encoding, Encoding::Utf16LeRaw | Encoding::Utf16BeRaw).then_some(encoding);
            docs.open_encoded(&path, explicit).unwrap();
            assert_eq!(docs.current().text, original);
            assert_eq!(docs.current().encoding, encoding);
            assert_eq!(docs.current().ending, LineEnding::Crlf);
            docs.save(None).unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
            let target = if encoding == Encoding::Utf8 {
                Encoding::Utf16Be
            } else {
                Encoding::Utf8
            };
            docs.current_mut().set_encoding(target).unwrap();
            assert!(docs.current().dirty());
            docs.save_all().unwrap();
            assert_eq!(
                read_file(&path, None).unwrap(),
                (original.to_owned(), target)
            );
            docs.close(false).unwrap();
            docs.reopen().unwrap();
            assert_eq!(docs.current().text, original);
        }
        let path = dir.path().join("invalid");
        for data in [
            &b"\xff\xfe\x00\xd8"[..],
            b"\xfe\xffx",
            b"\x80",
            b"\xff\xfe\0\0x\0\0\0",
        ] {
            fs::write(&path, data).unwrap();
            assert!(Documents::default().open(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), data);
        }
    }

    #[test]
    fn line_operations_keep_logical_cursor_selection_and_endings() {
        let mut doc = Document::empty(1);
        doc.text = "猫\r\nb\r\nlast".into();
        doc.text_changed();
        doc.ending = LineEnding::Crlf;
        doc.goto(3, 2).unwrap();
        doc.move_line(false).unwrap();
        assert_eq!(doc.text, "猫\r\nlast\r\nb");
        assert_eq!(doc.position(), (2, 2));
        doc.duplicate_line().unwrap();
        assert_eq!(doc.text, "猫\r\nlast\r\nlast\r\nb");
        doc.selection = Some(0..doc.line_starts[2]);
        doc.indent(true, true, 2).unwrap();
        assert_eq!(doc.text, "  猫\r\n  last\r\nlast\r\nb");
        doc.indent(false, true, 2).unwrap();
        doc.set_ending(LineEnding::Lf).unwrap();
        assert_eq!(doc.text, "猫\nlast\nlast\nb");
    }

    #[test]
    fn char_offsets_match_utf8_boundaries_after_edits() {
        let mut doc = Document::empty(0);
        for text in ["", "aé猫🦀\r\nb", "猫\n", "x\r", "\n\n"] {
            doc.text = text.to_owned();
            doc.text_changed();
            for (index, (byte, _)) in text.char_indices().enumerate() {
                assert_eq!(doc.byte_at_char(index), byte);
            }
            assert_eq!(doc.byte_at_char(text.chars().count()), text.len());
            assert_eq!(doc.byte_at_char(usize::MAX), text.len());
        }
    }

    #[test]
    fn restore_prefers_matching_file_to_unused_initial_tab() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes");
        fs::write(&path, "old").unwrap();
        let mut docs = Documents::default();
        docs.create();
        docs.open(&path).unwrap();
        docs.current_mut().text.push_str(" edit");
        docs.current_mut().text_changed();
        let draft = docs.current().draft();
        docs.reload(docs.current().id).unwrap();
        docs.restore_draft(draft);
        assert_eq!(docs.current().text, "old edit");
        assert_eq!(
            docs.items.iter().filter(|doc| doc.path.is_some()).count(),
            1
        );
    }

    #[test]
    fn read_only_document_can_only_be_saved_to_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original");
        fs::write(&path, "original").unwrap();
        let original = fs::metadata(&path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&path, readonly).unwrap();
        let mut docs = Documents::default();
        docs.open(&path).unwrap();
        docs.current_mut().text.push('!');
        docs.current_mut().text_changed();
        assert!(docs.save(None).is_err());
        assert!(docs.current().dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        let copy = dir.path().join("copy");
        docs.save(Some(&copy)).unwrap();
        assert_eq!(fs::read_to_string(copy).unwrap(), "original!");
        fs::set_permissions(path, original).unwrap();
    }

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
        assert_eq!(document.encoding, Encoding::Utf8Bom);
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
