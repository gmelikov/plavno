use anyhow::{Context, Result, ensure};
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    execute,
    terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
};
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub enum UpdateMode {
    Full,
    #[default]
    Diff,
}

impl CellSize {
    pub fn dance_columns(self, columns: u16) -> u16 {
        // The rig's stage width is expressed in Braille columns (two dots by four).
        // Native square pixels need the equivalent width for the actual font aspect.
        let width = u32::from(columns.saturating_sub(6)) * 2 * u32::from(self.width)
            / u32::from(self.height);
        (width.min(u32::from(u16::MAX - 6)) as u16).saturating_add(6)
    }

    pub fn read() -> Result<Self> {
        let size =
            crossterm::terminal::window_size().context("Cannot read terminal pixel dimensions")?;
        ensure!(
            size.columns > 0
                && size.rows > 0
                && size.width >= size.columns
                && size.height >= size.rows,
            "Sixel needs terminal pixel dimensions; try running directly in foot without a multiplexer"
        );
        let cell = Self {
            width: size.width / size.columns,
            height: size.height / size.rows,
        };
        ensure!(
            cell.width <= 64 && cell.height <= 128,
            "Unsupported terminal cell dimensions"
        );
        Ok(cell)
    }
}

pub struct Image {
    pub area: Rect,
    pub width: usize,
    pub height: usize,
    pub palette: [[u8; 3]; 8],
    // Color zero is transparent, not a painted background.
    pub pixels: Vec<u8>,
}

impl Image {
    #[cfg(test)]
    fn pixel(&self, x: usize, y: usize, cell: CellSize) -> Option<[u8; 3]> {
        let x = x.checked_sub(usize::from(self.area.x) * usize::from(cell.width))?;
        let y = y.checked_sub(usize::from(self.area.y) * usize::from(cell.height))?;
        if x >= self.width || y >= self.height {
            return None;
        }
        let color = self.pixels[y * self.width + x];
        (color != 0).then(|| self.palette[usize::from(color)])
    }

    fn cell_rows(&self, x: u16, y: u16, cell: CellSize) -> Option<impl Iterator<Item = &[u8]>> {
        if !self.area.contains((x, y).into()) {
            return None;
        }
        let width = usize::from(cell.width);
        let height = usize::from(cell.height);
        let start = usize::from(y - self.area.y) * height * self.width
            + usize::from(x - self.area.x) * width;
        Some(
            self.pixels[start..]
                .chunks(self.width)
                .take(height)
                .map(move |row| &row[..width]),
        )
    }

    fn crop(&self, area: Rect, cell: CellSize) -> Option<Self> {
        let area = self.area.intersection(area);
        if area.is_empty() {
            return None;
        }
        let width = usize::from(area.width) * usize::from(cell.width);
        let height = usize::from(area.height) * usize::from(cell.height);
        let left = usize::from(area.x - self.area.x) * usize::from(cell.width);
        let top = usize::from(area.y - self.area.y) * usize::from(cell.height);
        let mut pixels = Vec::with_capacity(width * height);
        for y in top..top + height {
            pixels.extend_from_slice(
                &self.pixels[y * self.width + left..y * self.width + left + width],
            );
        }
        Some(Self {
            area,
            width,
            height,
            palette: self.palette,
            pixels,
        })
    }

    pub fn protect_text(&mut self, buffer: &Buffer, cell: CellSize) {
        for y in 0..self.area.height {
            for x in buffer.area.x..self.area.right() {
                let symbol = buffer[(x, self.area.y + y)].symbol();
                // Upper half blocks are the album background, not foreground text.
                if symbol
                    .chars()
                    .all(|ch| ch == ' ' || ch == '▀' || ('\u{2800}'..='\u{28ff}').contains(&ch))
                {
                    continue;
                }
                let span = Rect::new(x, self.area.y + y, symbol.width().max(1) as u16, 1)
                    .intersection(self.area);
                for px in span.x..span.right() {
                    for py in usize::from(y * cell.height)..usize::from((y + 1) * cell.height) {
                        let start = py * self.width + usize::from((px - self.area.x) * cell.width);
                        self.pixels[start..start + usize::from(cell.width)].fill(0);
                    }
                }
            }
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        // Square pixels, transparent background, explicit raster bounds.
        write!(output, "\x1bP7;1;0q\"1;1;{};{}", self.width, self.height).unwrap();
        for (index, rgb) in self.palette.iter().enumerate().skip(1) {
            let [r, g, b] = rgb.map(|value| (u16::from(value) * 100 + 127) / 255);
            write!(output, "#{index};2;{r};{g};{b}").unwrap();
        }
        let colors = self.palette.len() - 1;
        let mut planes = vec![0_u8; self.width * colors];
        for band in (0..self.height).step_by(6) {
            planes.fill(0);
            let mut ends = [0; 7];
            // Read each pixel once to build all palette planes for this band.
            for y in band..(band + 6).min(self.height) {
                let bit = 1 << (y - band);
                for (x, &color) in self.pixels[y * self.width..(y + 1) * self.width]
                    .iter()
                    .enumerate()
                {
                    if color != 0 {
                        let color = usize::from(color - 1);
                        planes[color * self.width + x] |= bit;
                        ends[color] = ends[color].max(x + 1);
                    }
                }
            }
            let mut first = true;
            for (color, &end) in ends.iter().enumerate() {
                if end == 0 {
                    continue;
                }
                if !first {
                    output.push(b'$');
                }
                first = false;
                write!(output, "#{}", color + 1).unwrap();
                let columns = &planes[color * self.width..color * self.width + end];
                let mut x = 0;
                while x < end {
                    let value = columns[x];
                    let mut count = 1;
                    while x + count < end && columns[x + count] == value {
                        count += 1;
                    }
                    if count >= 4 {
                        write!(output, "!{count}").unwrap();
                        output.push(b'?' + value);
                    } else {
                        output.extend(std::iter::repeat_n(b'?' + value, count));
                    }
                    x += count;
                }
            }
            if band + 6 < self.height {
                output.push(b'-');
            }
        }
        output.extend_from_slice(b"\x1b\\");
        output
    }
}

struct Previous {
    image: Image,
    underlay: Buffer,
}

pub struct Presenter {
    mode: UpdateMode,
    previous: Option<Previous>,
    reset: bool,
}

impl Presenter {
    pub fn new(mode: UpdateMode) -> Self {
        Self {
            mode,
            previous: None,
            reset: true,
        }
    }

    pub fn active(&self) -> bool {
        self.previous.is_some()
    }

    pub fn invalidate(&mut self) {
        self.reset = true;
    }

    pub fn full_redraw(&self) -> bool {
        self.mode == UpdateMode::Full || self.reset
    }

    fn changed_area(&self, image: Option<&Image>, buffer: &Buffer, cell: CellSize) -> Rect {
        let previous = self.previous.as_ref();
        let old = previous.map(|previous| &previous.image);
        let bounds = match (old, image) {
            (Some(old), Some(new)) => old.area.union(new.area),
            (Some(old), None) => old.area,
            (None, Some(new)) => new.area,
            (None, None) => return Rect::default(),
        }
        .intersection(buffer.area);
        let same_palette = old.zip(image).is_some_and(|(a, b)| a.palette == b.palette);
        let mut changed: Option<Rect> = None;
        for y in bounds.y..bounds.bottom() {
            for x in bounds.x..bounds.right() {
                let text_changed = previous
                    .is_some_and(|previous| previous.underlay.cell((x, y)) != buffer.cell((x, y)));
                let before =
                    old.and_then(|image| image.cell_rows(x, y, cell).map(|rows| (image, rows)));
                let after =
                    image.and_then(|image| image.cell_rows(x, y, cell).map(|rows| (image, rows)));
                let differs = match (before, after) {
                    (Some((old, before)), Some((new, after))) => before.zip(after).any(|(a, b)| {
                        if text_changed && a.iter().any(|&color| color != 0) {
                            return true;
                        }
                        if same_palette && a == b {
                            return false;
                        }
                        a.iter().zip(b).any(|(&a, &b)| {
                            let a = (a != 0).then(|| old.palette[usize::from(a)]);
                            let b = (b != 0).then(|| new.palette[usize::from(b)]);
                            a != b
                        })
                    }),
                    (Some((_, mut rows)), None) | (None, Some((_, mut rows))) => {
                        rows.any(|row| row.iter().any(|&color| color != 0))
                    }
                    (None, None) => false,
                };
                if differs {
                    let cell = Rect::new(x, y, 1, 1);
                    changed = Some(changed.map_or(cell, |area| area.union(cell)));
                }
            }
        }
        changed.map_or(Rect::default(), |area| expand_wide(area, buffer))
    }

    fn present(
        &mut self,
        writer: &mut impl Write,
        image: Option<Image>,
        buffer: &Buffer,
        cell: CellSize,
    ) -> io::Result<()> {
        if self.full_redraw() {
            if let Some(image) = &image {
                write_image(writer, image)?;
            }
        } else {
            let area = self.changed_area(image.as_ref(), buffer, cell);
            if !area.is_empty() {
                // Transparent sixels blend with old pixels. Repainting the text
                // cells removes those pixels and restores the wave/art underneath.
                let mut underlay = copy_area(buffer, area);
                for cell in &mut underlay.content {
                    cell.diff_option = CellDiffOption::AlwaysUpdate;
                }
                CrosstermBackend::new(&mut *writer)
                    .draw(Buffer::empty(area).diff_iter(&underlay))?;
                if let Some(patch) = image.as_ref().and_then(|image| image.crop(area, cell)) {
                    write_image(writer, &patch)?;
                }
            }
        }
        writer.flush()?;
        self.previous = image.map(|image| Previous {
            underlay: copy_area(buffer, image.area),
            image,
        });
        self.reset = false;
        Ok(())
    }
}

fn copy_area(buffer: &Buffer, area: Rect) -> Buffer {
    let mut copy = Buffer::empty(area);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            copy[(x, y)].clone_from(&buffer[(x, y)]);
        }
    }
    copy
}

fn expand_wide(mut area: Rect, buffer: &Buffer) -> Rect {
    let (mut left, mut right) = (area.x, area.right());
    loop {
        let before = (left, right);
        for y in area.y..area.bottom() {
            let mut x = buffer.area.x;
            while x < buffer.area.right() {
                let end = x
                    .saturating_add(buffer[(x, y)].symbol().width().max(1) as u16)
                    .min(buffer.area.right());
                if x < right && end > left {
                    left = left.min(x);
                    right = right.max(end);
                }
                x = end;
            }
        }
        if before == (left, right) {
            break;
        }
    }
    area.x = left;
    area.width = right - left;
    area
}

fn write_image(writer: &mut impl Write, image: &Image) -> io::Result<()> {
    execute!(writer, MoveTo(image.area.x, image.area.y))?;
    writer.write_all(&image.encode())
}

// Keep text and graphics in one synchronized update in either presentation mode.
pub struct FrameGuard {
    saved: bool,
}

impl FrameGuard {
    pub fn begin(full: bool) -> io::Result<Self> {
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        if full {
            execute!(io::stdout(), Clear(ClearType::All))?;
        }
        Ok(Self { saved: false })
    }

    pub fn present(
        &mut self,
        presenter: &mut Presenter,
        image: Option<Image>,
        buffer: &Buffer,
        cell: CellSize,
    ) -> io::Result<()> {
        execute!(io::stdout(), SavePosition)?;
        self.saved = true;
        presenter.present(&mut io::stdout().lock(), image, buffer, cell)
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if self.saved {
            let _ = execute!(io::stdout(), RestorePosition);
        }
        let _ = execute!(io::stdout(), EndSynchronizedUpdate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Model transparent sixels layered over cell-based text erasure, as in foot.
    struct PixelTerminal {
        width: usize,
        cell: CellSize,
        pixels: Vec<Option<[u8; 3]>>,
        cursor: (usize, usize),
    }

    impl PixelTerminal {
        fn apply(&mut self, data: &[u8]) {
            let mut text = std::str::from_utf8(data).unwrap();
            while !text.is_empty() {
                if text.starts_with("\x1bP") {
                    let end = text.find("\x1b\\").unwrap() + 2;
                    let encoded = &text[..end];
                    let dimensions: Vec<usize> = encoded
                        .split('"')
                        .nth(1)
                        .unwrap()
                        .split('#')
                        .next()
                        .unwrap()
                        .split(';')
                        .map(|value| value.parse().unwrap())
                        .collect();
                    let (width, height) = (dimensions[2], dimensions[3]);
                    let mut palette = [[0; 3]; 8];
                    let colors = regex_lite::Regex::new(r"#(\d+);2;(\d+);(\d+);(\d+)").unwrap();
                    for color in colors.captures_iter(encoded) {
                        palette[color[1].parse::<usize>().unwrap()] =
                            std::array::from_fn(|index| color[index + 2].parse().unwrap());
                    }
                    for (index, color) in decode(encoded.as_bytes(), width, height)
                        .into_iter()
                        .enumerate()
                    {
                        if color != 0 {
                            let x = self.cursor.0 * usize::from(self.cell.width) + index % width;
                            let y = self.cursor.1 * usize::from(self.cell.height) + index / width;
                            self.pixels[y * self.width + x] = Some(palette[usize::from(color)]);
                        }
                    }
                    text = &text[end..];
                } else if let Some(sequence) = text.strip_prefix("\x1b[") {
                    let end = sequence.find(|ch: char| ('@'..='~').contains(&ch)).unwrap();
                    if sequence.as_bytes()[end] == b'H' {
                        let mut position = sequence[..end]
                            .split(';')
                            .map(|value| value.parse::<usize>().unwrap() - 1);
                        self.cursor.1 = position.next().unwrap();
                        self.cursor.0 = position.next().unwrap();
                    }
                    text = &sequence[end + 1..];
                } else {
                    let ch = text.chars().next().unwrap();
                    let columns = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    for y in self.cursor.1 * usize::from(self.cell.height)
                        ..(self.cursor.1 + 1) * usize::from(self.cell.height)
                    {
                        let left = y * self.width + self.cursor.0 * usize::from(self.cell.width);
                        self.pixels[left..left + columns * usize::from(self.cell.width)].fill(None);
                    }
                    self.cursor.0 += columns;
                    text = &text[ch.len_utf8()..];
                }
            }
        }
    }

    fn fixture_image(area: Rect, cell: CellSize, phase: usize) -> Image {
        let width = usize::from(area.width) * usize::from(cell.width);
        let height = usize::from(area.height) * usize::from(cell.height);
        Image {
            area,
            width,
            height,
            palette: std::array::from_fn(|index| {
                [
                    if index & 1 != 0 { 255 } else { 0 },
                    if index & 2 != 0 { 255 } else { 0 },
                    if index & 4 != 0 { 255 } else { 0 },
                ]
            }),
            pixels: (0..width * height)
                .map(|index| ((index + phase) % 8) as u8)
                .collect(),
        }
    }

    #[test]
    fn diff_erases_moving_and_removed_pixels_and_restores_graphics_after_text_updates() {
        let cell = CellSize {
            width: 2,
            height: 3,
        };
        let mut presenter = Presenter::new(UpdateMode::Diff);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 8));
        let mut terminal = PixelTerminal {
            width: 40,
            cell,
            pixels: vec![None; 40 * 24],
            cursor: (0, 0),
        };
        for frame in 0..30 {
            let old = buffer.clone();
            buffer[(4, 2)].set_char(if frame % 2 == 0 { '⠁' } else { '⠂' });
            buffer[(5, 3)].set_char(if frame % 3 == 0 { 'X' } else { ' ' });
            let mut image = (!matches!(frame, 8 | 9 | 17)).then(|| {
                let mut image =
                    fixture_image(Rect::new(2 + (frame / 6) as u16, 1, 6, 4), cell, frame / 2);
                if frame % 4 == 0 {
                    image.palette.swap(1, 2);
                }
                image.protect_text(&buffer, cell);
                image
            });
            let expected: Vec<_> = (0..40 * 24)
                .map(|index| {
                    image
                        .as_ref()
                        .and_then(|image| image.pixel(index % 40, index / 40, cell))
                        .map(|rgb| rgb.map(|value| ((u16::from(value) * 100 + 127) / 255) as u8))
                })
                .collect();
            if frame == 20 {
                presenter.invalidate();
                assert!(presenter.full_redraw());
                terminal.pixels.fill(None);
            }
            let mut output = Vec::new();
            CrosstermBackend::new(&mut output)
                .draw(old.diff_iter(&buffer))
                .unwrap();
            presenter
                .present(&mut output, image.take(), &buffer, cell)
                .unwrap();
            terminal.apply(&output);
            assert_eq!(
                terminal.pixels, expected,
                "Stale or missing pixels at frame {frame}"
            );
        }
    }

    #[test]
    fn diff_skips_identical_frames_and_crops_a_single_cell_change() {
        let cell = CellSize {
            width: 2,
            height: 3,
        };
        let area = Rect::new(2, 1, 6, 4);
        let buffer = Buffer::empty(Rect::new(0, 0, 20, 8));
        let mut presenter = Presenter::new(UpdateMode::Diff);
        presenter
            .present(
                &mut Vec::new(),
                Some(fixture_image(area, cell, 0)),
                &buffer,
                cell,
            )
            .unwrap();
        let image = fixture_image(area, cell, 0);
        assert!(
            presenter
                .changed_area(Some(&image), &buffer, cell)
                .is_empty()
        );
        let mut output = Vec::new();
        presenter
            .present(&mut output, Some(image), &buffer, cell)
            .unwrap();
        assert!(output.is_empty());

        let mut image = fixture_image(area, cell, 0);
        image.pixels[1] = 7;
        let changed = presenter.changed_area(Some(&image), &buffer, cell);
        assert_eq!(changed, Rect::new(2, 1, 1, 1));
        let cropped = image.crop(changed, cell).unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 3));
        assert_eq!(decode(&cropped.encode(), 2, 3), cropped.pixels);
    }

    #[test]
    fn diff_compares_visible_colors_and_preserves_transparency() {
        let cell = CellSize {
            width: 2,
            height: 3,
        };
        let area = Rect::new(2, 1, 3, 1);
        let buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        let make_image = || {
            let mut image = fixture_image(area, cell, 0);
            image.palette[1] = [0; 3];
            image.palette[2] = [0; 3];
            image.pixels.fill(1);
            for row in image.pixels.chunks_mut(image.width) {
                row[..usize::from(cell.width)].fill(0);
            }
            image
        };
        let mut presenter = Presenter::new(UpdateMode::Diff);
        presenter
            .present(&mut Vec::new(), Some(make_image()), &buffer, cell)
            .unwrap();

        let mut image = make_image();
        for pixel in &mut image.pixels {
            if *pixel == 1 {
                *pixel = 2;
            }
        }
        assert!(
            presenter
                .changed_area(Some(&image), &buffer, cell)
                .is_empty()
        );
        image.palette[7] = [17, 42, 90];
        assert!(
            presenter
                .changed_area(Some(&image), &buffer, cell)
                .is_empty()
        );

        // Opaque black must repaint a previously transparent pixel.
        image.pixels[0] = 2;
        assert_eq!(
            presenter.changed_area(Some(&image), &buffer, cell),
            Rect::new(2, 1, 1, 1)
        );
    }

    #[test]
    fn wide_text_at_patch_edges_is_repainted_as_whole_glyphs() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer[(4, 0)].set_char('界');
        buffer[(6, 0)].set_symbol("👩‍🎤");
        buffer[(5, 1)].set_char('界');
        assert_eq!(
            expand_wide(Rect::new(6, 0, 1, 2), &buffer),
            Rect::new(4, 0, 4, 2)
        );
    }

    #[test]
    fn full_mode_emits_each_frame_and_always_requests_a_screen_repaint() {
        let cell = CellSize {
            width: 2,
            height: 3,
        };
        let area = Rect::new(2, 1, 6, 4);
        let buffer = Buffer::empty(Rect::new(0, 0, 20, 8));
        let mut presenter = Presenter::new(UpdateMode::Full);
        for _ in 0..2 {
            assert!(presenter.full_redraw());
            let mut output = Vec::new();
            presenter
                .present(
                    &mut output,
                    Some(fixture_image(area, cell, 0)),
                    &buffer,
                    cell,
                )
                .unwrap();
            assert!(output.windows(8).any(|bytes| bytes == b"\x1bP7;1;0q"));
            assert!(presenter.active());
        }
        presenter
            .present(&mut Vec::new(), None, &buffer, cell)
            .unwrap();
        assert!(!presenter.active());
    }

    fn decode(data: &[u8], width: usize, height: usize) -> Vec<u8> {
        assert!(data.starts_with(b"\x1bP7;1;0q\"1;1;"));
        assert!(data.ends_with(b"\x1b\\"));
        let mut index = data.iter().position(|&ch| ch == b'q').unwrap() + 1;
        let mut pixels = vec![0; width * height];
        let (mut x, mut y, mut color) = (0, 0, 0);
        let number = |index: &mut usize| {
            let mut value = 0;
            while data[*index].is_ascii_digit() {
                value = value * 10 + usize::from(data[*index] - b'0');
                *index += 1;
            }
            value
        };
        while index < data.len() - 2 {
            let ch = data[index];
            index += 1;
            match ch {
                b'"' => {
                    for _ in 0..4 {
                        number(&mut index);
                        if data[index] == b';' {
                            index += 1;
                        }
                    }
                }
                b'#' => {
                    color = number(&mut index) as u8;
                    if data[index] == b';' {
                        for _ in 0..4 {
                            index += 1;
                            number(&mut index);
                        }
                    }
                }
                b'$' => x = 0,
                b'-' => {
                    x = 0;
                    y += 6;
                }
                _ => {
                    let (count, bits) = if ch == b'!' {
                        let count = number(&mut index);
                        let bits = data[index] - b'?';
                        index += 1;
                        (count, bits)
                    } else {
                        assert!((b'?'..=b'~').contains(&ch));
                        (1, ch - b'?')
                    };
                    for _ in 0..count {
                        for bit in 0..6 {
                            if bits & (1 << bit) != 0 {
                                assert!(x < width && y + bit < height);
                                pixels[(y + bit) * width + x] = color;
                            }
                        }
                        x += 1;
                    }
                }
            }
        }
        pixels
    }

    #[test]
    fn sixel_round_trips_colors_transparency_partial_bands_and_long_runs() {
        for (width, height) in [(1, 1), (13, 11), (300, 18)] {
            let mut pixels: Vec<_> = (0..width * height)
                .map(|index| ((index * 7 + index / 3) % 8) as u8)
                .collect();
            if height >= 18 {
                pixels[width * 6..width * 12].fill(0);
                pixels[width * 12..].fill(6);
            }
            let image = Image {
                area: Rect::default(),
                width,
                height,
                palette: [[90, 160, 240]; 8],
                pixels,
            };
            let encoded = image.encode();
            assert_eq!(decode(&encoded, width, height), image.pixels);
            assert!(!encoded.windows(5).any(|bytes| bytes == b"#0;2;"));
            if width == 300 {
                assert!(encoded.windows(4).any(|bytes| bytes == b"!300"));
            }
        }
    }

    #[test]
    fn album_art_half_blocks_do_not_mask_the_sixel_dancer() {
        let mut image = Image {
            area: Rect::new(0, 0, 3, 2),
            width: 12,
            height: 12,
            palette: [[0; 3]; 8],
            pixels: (0..144).map(|index| (index % 8) as u8).collect(),
        };
        let original = image.pixels.clone();
        let mut buffer = Buffer::empty(image.area);
        for cell in &mut buffer.content {
            cell.set_char('▀')
                .set_fg(ratatui::style::Color::Rgb(20, 30, 40))
                .set_bg(ratatui::style::Color::Rgb(40, 30, 20));
        }
        image.protect_text(
            &buffer,
            CellSize {
                width: 4,
                height: 6,
            },
        );
        assert_eq!(image.pixels, original, "Album artwork removes the dancer");
    }

    #[test]
    fn text_and_wide_character_continuations_take_priority_over_graphics() {
        let mut image = Image {
            area: Rect::new(1, 0, 3, 2),
            width: 12,
            height: 12,
            palette: [[0; 3]; 8],
            pixels: vec![3; 144],
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        buffer[(0, 0)].set_symbol("界"); // Starts outside the image but extends into it.
        buffer[(2, 0)].set_char('P');
        buffer[(3, 0)].set_char('⠿');
        image.protect_text(
            &buffer,
            CellSize {
                width: 4,
                height: 6,
            },
        );
        for y in 0..12 {
            for x in 0..12 {
                assert_eq!(image.pixels[y * 12 + x], if y < 6 && x < 8 { 0 } else { 3 });
            }
        }
        let empty = Buffer::empty(buffer.area);
        let diff = empty.diff(&buffer);
        assert!(
            diff.iter()
                .any(|(x, y, cell)| *x == 0 && *y == 0 && cell.symbol() == "界")
        );
        assert!(!diff.iter().any(|(x, y, _)| *x == 1 && *y == 0));
    }
}
