use std::{
    collections::{HashMap, VecDeque},
    io::{self, BufRead, BufReader, Cursor, Read, Seek},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{self, ChildEvent, EventedPty, EventedReadWrite},
};
use base64::{
    Engine, alphabet,
    engine::{
        DecodePaddingMode,
        general_purpose::{GeneralPurpose, GeneralPurposeConfig},
    },
};
use eframe::egui::{
    self, Color32, ColorImage, Context, Painter, Rect, TextureHandle, TextureOptions, Vec2,
};
use polling::{Event, PollMode, Poller};

const KITTY_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub const MARKER: &str = "Synth:KittyGraphics";
const REPORT_POSITION: &[u8] = b"\x1b[22t\x1b]2;Synth:KittyGraphics\x07\x1b[6n\x1b[23t";
const MAX_FRAME: usize = 128 * 1024;
const MAX_PACKET: usize = 12 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TEXTURE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PLACEMENTS: usize = 8192;
const MAX_PENDING_BYTES: usize = 24 * 1024 * 1024;

#[derive(Default)]
enum ParseState {
    #[default]
    Text,
    Escape,
    Apc,
    Graphics,
    GraphicsEscape,
}

#[derive(Default)]
struct GraphicsParser {
    state: ParseState,
    packet: Vec<u8>,
    oversized: bool,
}

impl GraphicsParser {
    fn accept(
        &mut self,
        bytes: &[u8],
        output: &mut VecDeque<u8>,
        mut packet: impl FnMut(Vec<u8>, &mut VecDeque<u8>),
    ) {
        use ParseState::*;
        for &byte in bytes {
            self.state = match (&self.state, byte) {
                (Text, 0x1b) => Escape,
                (Text, _) => {
                    output.push_back(byte);
                    Text
                }
                (Escape, b'_') => Apc,
                (Escape, 0x1b) => {
                    output.push_back(0x1b);
                    Escape
                }
                (Escape, _) => {
                    output.extend([0x1b, byte]);
                    Text
                }
                (Apc, b'G') => Graphics,
                (Apc, 0x1b) => {
                    output.extend(b"\x1b_");
                    Escape
                }
                (Apc, _) => {
                    output.extend([0x1b, b'_', byte]);
                    Text
                }
                (Graphics, 0x1b) => GraphicsEscape,
                (Graphics, 0x18 | 0x1a) => {
                    self.reset();
                    Text
                }
                (Graphics, _) => {
                    self.push(byte);
                    Graphics
                }
                (GraphicsEscape, b'\\') => {
                    if !self.oversized {
                        packet(std::mem::take(&mut self.packet), output);
                    }
                    self.reset();
                    Text
                }
                (GraphicsEscape, 0x1b) => {
                    self.push(0x1b);
                    GraphicsEscape
                }
                (GraphicsEscape, _) => {
                    self.push(0x1b);
                    self.push(byte);
                    Graphics
                }
            };
        }
    }

    fn push(&mut self, byte: u8) {
        if self.packet.len() < MAX_FRAME {
            self.packet.push(byte);
        } else {
            self.oversized = true;
        }
    }

    fn reset(&mut self) {
        self.packet.clear();
        self.oversized = false;
    }
}

pub struct GraphicsPty {
    pty: tty::Pty,
    parser: GraphicsParser,
    output: VecDeque<u8>,
    packets: mpsc::Sender<PendingTransfer>,
    pending_bytes: Arc<AtomicUsize>,
    size: WindowSize,
    assembler: Option<Transfer>,
}

impl GraphicsPty {
    pub fn new(pty: tty::Pty, packets: mpsc::Sender<PendingTransfer>, size: WindowSize) -> Self {
        Self {
            pty,
            parser: GraphicsParser::default(),
            output: VecDeque::new(),
            packets,
            pending_bytes: Arc::default(),
            size,
            assembler: None,
        }
    }
}

impl Read for GraphicsPty {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut raw = [0; 8192];
        while self.output.is_empty() {
            let count = self.pty.reader().read(&mut raw)?;
            if count == 0 {
                return Ok(0);
            }
            self.parser
                .accept(&raw[..count], &mut self.output, |packet, output| {
                    if let Some(transfer) = assemble(&mut self.assembler, &packet)
                        && let Some(pending) = PendingTransfer::new(transfer, &self.pending_bytes)
                    {
                        let cursor_movement = pending.transfer.cursor_movement(self.size);
                        if self.packets.send(pending).is_ok() {
                            output.extend(REPORT_POSITION);
                            output.extend(cursor_movement.bytes());
                        }
                    }
                });
        }
        let count = buffer.len().min(self.output.len());
        for (target, byte) in buffer[..count].iter_mut().zip(self.output.drain(..count)) {
            *target = byte;
        }
        Ok(count)
    }
}

impl EventedReadWrite for GraphicsPty {
    type Reader = Self;
    type Writer = <tty::Pty as EventedReadWrite>::Writer;

    unsafe fn register(
        &mut self,
        poller: &Arc<Poller>,
        event: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        unsafe { self.pty.register(poller, event, mode) }
    }

    fn reregister(&mut self, poller: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        self.pty.reregister(poller, event, mode)
    }

    fn deregister(&mut self, poller: &Arc<Poller>) -> io::Result<()> {
        self.pty.deregister(poller)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        self
    }
    fn writer(&mut self) -> &mut Self::Writer {
        self.pty.writer()
    }
}

impl OnResize for GraphicsPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size);
        self.size = size;
    }
}

impl EventedPty for GraphicsPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.pty.next_child_event()
    }
}

pub fn reported_position(text: &str) -> Option<(usize, usize)> {
    let (row, column) = text
        .strip_prefix("\x1b[")?
        .strip_suffix('R')?
        .split_once(';')?;
    Some((
        row.parse::<usize>().ok()?.checked_sub(1)?,
        column.parse::<usize>().ok()?.checked_sub(1)?,
    ))
}

#[derive(Default)]
struct Metadata(HashMap<u8, String>);

impl Metadata {
    fn number(&self, key: u8) -> usize {
        self.get(key).parse().unwrap_or(0)
    }
    fn get(&self, key: u8) -> &str {
        self.0.get(&key).map_or("", String::as_str)
    }
}

pub struct PendingTransfer {
    transfer: Transfer,
    pending_bytes: Arc<AtomicUsize>,
    charge: usize,
}

impl PendingTransfer {
    fn new(transfer: Transfer, pending_bytes: &Arc<AtomicUsize>) -> Option<Self> {
        let metadata = transfer.metadata.0.values().map(String::len).sum::<usize>();
        let charge = transfer.data.capacity() + metadata + 512;
        pending_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                (used + charge <= MAX_PENDING_BYTES).then_some(used + charge)
            })
            .ok()?;
        Some(Self {
            transfer,
            pending_bytes: Arc::clone(pending_bytes),
            charge,
        })
    }
}

impl Drop for PendingTransfer {
    fn drop(&mut self) {
        self.pending_bytes.fetch_sub(self.charge, Ordering::Relaxed);
    }
}

struct Transfer {
    metadata: Metadata,
    data: Vec<u8>,
}

impl Transfer {
    fn cursor_movement(&self, size: WindowSize) -> String {
        if !matches!(self.metadata.get(b'a'), "T" | "p") || self.metadata.get(b'C') == "1" {
            return String::new();
        }
        let mut movement = String::new();
        let cells = |key, pixels, cell: u16| {
            let explicit = self.metadata.number(key);
            if explicit == 0 {
                self.metadata
                    .number(pixels)
                    .div_ceil(usize::from(cell.max(1)))
                    .min(1024)
            } else {
                explicit.min(1024)
            }
        };
        let rows = cells(b'r', b'v', size.cell_height).saturating_sub(1);
        let columns = cells(b'c', b's', size.cell_width);
        if rows > 0 {
            movement.push_str(&format!("\x1b[{rows}B"));
        }
        if columns > 0 {
            movement.push_str(&format!("\x1b[{columns}C"));
        }
        movement
    }
}

fn assemble(in_progress: &mut Option<Transfer>, packet: &[u8]) -> Option<Transfer> {
    let (fields, data) = match packet.iter().position(|byte| *byte == b';') {
        Some(index) => (&packet[..index], &packet[index + 1..]),
        None => (packet, &[][..]),
    };
    let fields = std::str::from_utf8(fields).ok()?;
    if fields.len() > 1024 {
        return None;
    }
    let mut metadata = Metadata::default();
    for field in fields.split(',').filter(|field| !field.is_empty()) {
        let (key, value) = field.split_once('=')?;
        if key.len() != 1 {
            return None;
        }
        metadata.0.insert(key.as_bytes()[0], value.to_owned());
    }
    let more = metadata.get(b'm') == "1";
    let mut transfer = in_progress.take().unwrap_or_else(|| Transfer {
        metadata,
        data: Vec::new(),
    });
    if transfer.data.len().saturating_add(data.len()) > MAX_PACKET {
        return None;
    }
    transfer.data.extend_from_slice(data);
    if more {
        *in_progress = Some(transfer);
        None
    } else {
        Some(transfer)
    }
}

struct Image {
    texture: TextureHandle,
    width: usize,
    height: usize,
}

struct Placement {
    image: usize,
    id: usize,
    row: i64,
    column: usize,
    offset: Vec2,
    cells: Vec2,
    source: Rect,
    z: i32,
    alternate: bool,
}

#[derive(Default)]
pub struct Graphics {
    images: HashMap<usize, Image>,
    placements: Vec<Placement>,
    image_number: usize,
}

#[derive(Clone, Copy)]
pub struct Position {
    pub row: i64,
    pub column: usize,
    pub alternate: bool,
}

impl Graphics {
    pub fn receive(
        &mut self,
        ctx: &Context,
        pending: PendingTransfer,
        position: Position,
    ) -> Option<String> {
        let Transfer { metadata, data } = &pending.transfer;
        let requested = metadata.number(b'i');
        let action = metadata.get(b'a');
        let result = match action {
            "p" => self.place(metadata, requested, position),
            "d" => {
                self.delete(metadata, position.alternate);
                Ok(())
            }
            "" | "t" | "T" | "q" => self.transmit(ctx, metadata, data, position),
            _ => Err("ENOTSUP: Graphics action is unsupported"),
        };
        reply(metadata, result)
    }

    fn transmit(
        &mut self,
        ctx: &Context,
        metadata: &Metadata,
        bytes: &[u8],
        position: Position,
    ) -> Result<(), &'static str> {
        if !matches!(metadata.get(b't'), "" | "d") {
            return Err("ENOTSUP: Use direct stream transfer");
        }
        let image = decode(metadata, bytes)?;
        if metadata.get(b'a') == "q" {
            return Ok(());
        }
        let id = self.upload(ctx, metadata.number(b'i'), image)?;
        if metadata.get(b'a') == "T" {
            self.place(metadata, id, position)?;
        }
        Ok(())
    }

    fn upload(
        &mut self,
        ctx: &Context,
        requested: usize,
        image: ColorImage,
    ) -> Result<usize, &'static str> {
        let mut id = requested;
        if id == 0 {
            loop {
                self.image_number = self.image_number.wrapping_add(1);
                id = self.image_number;
                if id != 0 && !self.images.contains_key(&id) {
                    break;
                }
            }
        }
        let [width, height] = image.size;
        let resident = self
            .images
            .iter()
            .filter(|(key, _)| **key != id)
            .map(|(_, image)| image.width * image.height * 4)
            .sum::<usize>();
        if resident + width * height * 4 > MAX_TEXTURE_BYTES {
            return Err("ENOSPC: Terminal image memory is full");
        }
        let texture = ctx.load_texture(
            format!("terminal graphic {id}"),
            image,
            TextureOptions::LINEAR,
        );
        self.images.insert(
            id,
            Image {
                texture,
                width,
                height,
            },
        );
        Ok(id)
    }

    pub fn show_file(
        &mut self,
        ctx: &Context,
        path: &Path,
        position: Position,
        cell: Vec2,
        room: Vec2,
    ) -> Result<usize, String> {
        let info = std::fs::metadata(path)
            .map_err(|error| format!("{}: {}", path.display(), crate::io_message(&error)))?;
        if !info.is_file() {
            return Err("Expected a regular image file".into());
        }
        if info.len() > MAX_IMAGE_BYTES as u64 {
            return Err("Image file exceeds the size limit".into());
        }
        let file = std::fs::File::open(path)
            .map_err(|error| format!("{}: {}", path.display(), crate::io_message(&error)))?;
        let reader = image::ImageReader::new(BufReader::new(file))
            .with_guessed_format()
            .map_err(|error| crate::io_message(&error))?;
        let content = decode_image(reader)?;
        let [width, height] = content.size;
        let available = room * cell;
        let scale = (available.x / width as f32)
            .min(available.y / height as f32)
            .min(1.0);
        let columns = ((width as f32 * scale / cell.x).floor() as usize).max(1);
        let rows = (columns as f32 * cell.x * height as f32 / width as f32 / cell.y)
            .ceil()
            .min(room.y) as usize;
        let id = self.upload(ctx, 0, content)?;
        let mut metadata = Metadata::default();
        metadata.0.insert(b'c', columns.to_string());
        if let Err(error) = self.place(&metadata, id, position) {
            self.images.remove(&id);
            return Err(error.into());
        }
        Ok(rows.max(1))
    }

    fn place(
        &mut self,
        metadata: &Metadata,
        image: usize,
        position: Position,
    ) -> Result<(), &'static str> {
        if metadata.get(b'U') == "1" {
            return Err("ENOTSUP: Unicode placeholders are unsupported");
        }
        let stored = self
            .images
            .get(&image)
            .ok_or("ENOENT: Image ID not found")?;
        let id = metadata.number(b'p');
        if id != 0 {
            self.placements
                .retain(|item| (item.image, item.id) != (image, id));
        }
        if self.placements.len() >= MAX_PLACEMENTS {
            return Err("ENOSPC: Too many image placements");
        }
        let x = metadata.number(b'x').min(stored.width - 1);
        let y = metadata.number(b'y').min(stored.height - 1);
        let extent = |key, from: usize, limit: usize| {
            let size = metadata.number(key);
            if size == 0 {
                limit
            } else {
                from.saturating_add(size).min(limit)
            }
        };
        let right = extent(b'w', x, stored.width);
        let bottom = extent(b'h', y, stored.height);
        let source = Rect::from_min_max(
            egui::pos2(x as f32, y as f32),
            egui::pos2(right as f32, bottom as f32),
        );
        self.placements.push(Placement {
            image,
            id,
            row: position.row,
            column: position.column,
            source,
            alternate: position.alternate,
            offset: egui::vec2(
                metadata.number(b'X').min(4096) as f32,
                metadata.number(b'Y').min(4096) as f32,
            ),
            cells: egui::vec2(
                metadata.number(b'c').min(1024) as f32,
                metadata.number(b'r').min(1024) as f32,
            ),
            z: metadata.get(b'z').parse().unwrap_or(0),
        });
        self.placements.sort_by_key(|placement| placement.z);
        Ok(())
    }

    fn delete(&mut self, metadata: &Metadata, alternate: bool) {
        let image = metadata.number(b'i');
        let placement = metadata.number(b'p');
        match metadata.get(b'd') {
            "i" | "I" => {
                self.placements
                    .retain(|item| item.image != image || (placement != 0 && item.id != placement));
                if metadata.get(b'd') == "I"
                    && !self.placements.iter().any(|item| item.image == image)
                {
                    self.images.remove(&image);
                }
            }
            "" | "a" | "A" => {
                self.placements.retain(|item| item.alternate != alternate);
                if metadata.get(b'd') == "A" {
                    self.images
                        .retain(|id, _| self.placements.iter().any(|item| item.image == *id));
                }
            }
            _ => {}
        }
    }

    pub fn draw(
        &self,
        painter: &Painter,
        rect: Rect,
        cell: Vec2,
        origin: Position,
        below_text: bool,
    ) {
        for item in &self.placements {
            if item.alternate != origin.alternate || (item.z < 0) != below_text {
                continue;
            }
            let Some(image) = self.images.get(&item.image) else {
                continue;
            };
            let size = item.source.size() / painter.ctx().pixels_per_point();
            let width = if item.cells.x > 0.0 {
                item.cells.x * cell.x
            } else if item.cells.y > 0.0 {
                item.cells.y * cell.y * size.x / size.y
            } else {
                size.x
            };
            let height = if item.cells.y > 0.0 {
                item.cells.y * cell.y
            } else {
                width * size.y / size.x
            };
            let offset = item.offset / painter.ctx().pixels_per_point();
            let pos = rect.min
                + egui::vec2(
                    item.column as f32 * cell.x,
                    (item.row - origin.row) as f32 * cell.y,
                )
                + offset;
            let destination = Rect::from_min_size(pos, egui::vec2(width, height));
            if !painter.clip_rect().intersects(destination) {
                continue;
            }
            let texture_size = egui::vec2(image.width as f32, image.height as f32);
            let uv = Rect::from_min_max(
                egui::pos2(
                    item.source.min.x / texture_size.x,
                    item.source.min.y / texture_size.y,
                ),
                egui::pos2(
                    item.source.max.x / texture_size.x,
                    item.source.max.y / texture_size.y,
                ),
            );
            painter.image(image.texture.id(), destination, uv, Color32::WHITE);
        }
    }
}

fn reply(metadata: &Metadata, result: Result<(), &'static str>) -> Option<String> {
    let image = metadata.number(b'i');
    let placement = metadata.number(b'p');
    if metadata.get(b'q') == "2" || (metadata.get(b'q') == "1" && result.is_ok()) || image == 0 {
        return None;
    }
    let address = if placement == 0 {
        format!("i={image}")
    } else {
        format!("i={image},p={placement}")
    };
    Some(format!(
        "\x1b_G{address};{}\x1b\\",
        result.err().unwrap_or("OK")
    ))
}

fn decode(metadata: &Metadata, input: &[u8]) -> Result<ColorImage, &'static str> {
    let mut bytes = KITTY_BASE64
        .decode(input)
        .map_err(|_| "EINVAL: Invalid base64 image data")?;
    match metadata.get(b'o') {
        "" => {}
        "z" => {
            let mut data = Vec::new();
            flate2::read::ZlibDecoder::new(&*bytes)
                .take((MAX_IMAGE_BYTES + 1) as u64)
                .read_to_end(&mut data)
                .map_err(|_| "EINVAL: Invalid zlib image data")?;
            if data.len() > MAX_IMAGE_BYTES {
                return Err("E2BIG: Image data exceeds the size limit");
            }
            bytes = data;
        }
        _ => return Err("ENOTSUP: Image compression is unsupported"),
    }
    match metadata.get(b'f') {
        "100" => decode_image(image::ImageReader::with_format(
            Cursor::new(bytes),
            image::ImageFormat::Png,
        )),
        "" | "32" | "24" => {
            let width = metadata.number(b's');
            let height = metadata.number(b'v');
            let rgba = metadata.get(b'f') != "24";
            let size = [width, height];
            if !(1..=4096).contains(&width)
                || !(1..=4096).contains(&height)
                || width * height * 4 > MAX_IMAGE_BYTES
            {
                return Err("EINVAL: Invalid image dimensions");
            }
            if bytes.len() != width * height * if rgba { 4 } else { 3 } {
                return Err("EINVAL: Unexpected raw image length");
            }
            Ok(if rgba {
                ColorImage::from_rgba_unmultiplied(size, &bytes)
            } else {
                ColorImage::from_rgb(size, &bytes)
            })
        }
        _ => Err("ENOTSUP: Image format is unsupported"),
    }
}

fn decode_image<R: BufRead + Seek>(
    mut reader: image::ImageReader<R>,
) -> Result<ColorImage, &'static str> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| "EINVAL: Invalid image or unsupported dimensions")?;
    if u64::from(image.width()) * u64::from(image.height()) * 4 > MAX_IMAGE_BYTES as u64 {
        return Err("E2BIG: Image data exceeds the size limit");
    }
    let image = image.into_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        &image,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_unpadded_graphics_data_written_by_kitten() {
        let packet = assemble(&mut None, b"a=T,f=32,s=1,v=1;/xAg/w").unwrap();
        let image = decode(&packet.metadata, &packet.data).unwrap();
        assert_eq!(image.pixels, [Color32::from_rgb(255, 16, 32)]);
    }

    #[test]
    fn graphics_packets_survive_fragmented_pty_reads_without_consuming_text() {
        let mut parser = GraphicsParser::default();
        let mut output = VecDeque::new();
        let mut current = None;
        let mut completed = Vec::new();
        for input in [
            b"before\x1b_".as_slice(),
            b"Ga=T,f=24,s=1,v=1,C=1,m=1;AAAA\x1b",
            b"\\\x1b_Gm=0;\x1b\\after",
        ] {
            parser.accept(input, &mut output, |packet, output| {
                if let Some(transfer) = assemble(&mut current, &packet) {
                    completed.push(transfer);
                    output.extend(REPORT_POSITION);
                }
            });
        }
        let transfer = &completed[0];
        let image = decode(&transfer.metadata, &transfer.data).unwrap();
        assert_eq!(image.size, [1, 1]);
        assert_eq!(image.pixels, [Color32::BLACK]);
        assert_eq!(
            output.into_iter().collect::<Vec<_>>(),
            [b"before", REPORT_POSITION, b"after"].concat()
        );
    }
}
