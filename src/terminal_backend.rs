use crossterm::{
    cursor::{MoveRight, MoveTo},
    queue,
    style::{Colored, Colors, Print, SetColors, SetUnderlineColor},
};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, IntoCrossterm, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::Color,
};
use std::{
    collections::{HashMap, hash_map::Entry},
    io::{self, Write},
};
use unicode_width::UnicodeWidthStr;

const END: usize = usize::MAX;

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
struct PaintStyle([u32; 3]);

impl PaintStyle {
    fn new(cell: &Cell) -> Self {
        Self([
            color_key(cell.fg),
            color_key(cell.bg),
            color_key(cell.underline_color),
        ])
    }
}

// Keep named and indexed colors distinct, just as Crossterm's color cache does.
fn color_key(color: Color) -> u32 {
    match color {
        Color::Reset => 0,
        Color::Indexed(n) => 0x1000000 | u32::from(n),
        Color::Rgb(r, g, b) => {
            0x2000000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        }
        color => {
            0x3000000
                | match color {
                    Color::Black => 0,
                    Color::Red => 1,
                    Color::Green => 2,
                    Color::Yellow => 3,
                    Color::Blue => 4,
                    Color::Magenta => 5,
                    Color::Cyan => 6,
                    Color::Gray => 7,
                    Color::DarkGray => 8,
                    Color::LightRed => 9,
                    Color::LightGreen => 10,
                    Color::LightYellow => 11,
                    Color::LightBlue => 12,
                    Color::LightMagenta => 13,
                    Color::LightCyan => 14,
                    Color::White => 15,
                    _ => unreachable!(),
                }
        }
    }
}

fn digits(mut n: usize) -> usize {
    let mut result = 1;
    while n >= 10 {
        n /= 10;
        result += 1;
    }
    result
}

fn color_bytes(key: u32) -> u8 {
    if key == 0 {
        2
    } else if key >> 24 == 2 {
        (7 + digits(((key >> 16) & 255) as usize)
            + digits(((key >> 8) & 255) as usize)
            + digits((key & 255) as usize)) as u8
    } else {
        (5 + digits((key & 255) as usize)) as u8
    }
}

struct PendingCell {
    position: Position,
    cell: Cell,
    single_width: bool,
    next: usize,
    group: usize,
}

#[derive(Clone, Copy)]
struct CellRef<'a> {
    position: Position,
    cell: &'a Cell,
    single_width: bool,
}

impl PendingCell {
    fn as_ref(&self) -> CellRef<'_> {
        CellRef {
            position: self.position,
            cell: &self.cell,
            single_width: self.single_width,
        }
    }
}

struct Group {
    style: PaintStyle,
    first: usize,
    last: usize,
    lengths: [u8; 3],
}

#[derive(Default)]
struct FramePlan {
    cells: Vec<PendingCell>,
    map: HashMap<PaintStyle, usize>,
    groups: Vec<Group>,
    order: Vec<usize>,
    groupable: bool,
}

impl FramePlan {
    fn collect<'a>(&mut self, content: impl Iterator<Item = (u16, u16, &'a Cell)>) {
        self.cells.clear();
        self.groupable = true;
        let mut previous = None;
        for (x, y, cell) in content {
            let width = cell.symbol().width();
            self.groupable &= width == 1
                && cell.modifier.is_empty()
                && previous.is_none_or(|position| position < (y, x));
            previous = Some((y, x));
            let single_width = width == 1
                && cell
                    .symbol()
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.len_utf8() == cell.symbol().len());
            self.cells.push(PendingCell {
                position: Position::new(x, y),
                cell: cell.clone(),
                single_width,
                next: END,
                group: END,
            });
        }
    }

    fn prepare(&mut self) -> bool {
        if !self.groupable || self.cells.len() < 8 {
            return false;
        }
        self.map.clear();
        self.groups.clear();
        self.order.clear();
        let last = self.cells.len() - 1;
        for index in 0..self.cells.len() {
            let style = PaintStyle::new(&self.cells[index].cell);
            let group = match self.map.entry(style) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let group = self.groups.len();
                    self.groups.push(Group {
                        style,
                        first: END,
                        last: END,
                        lengths: [0; 3],
                    });
                    entry.insert(group);
                    group
                }
            };
            self.cells[index].group = group;
            // Re-emit the original last cell last, preserving cursor and pending
            // right-edge wrap state without relying on terminal dimensions.
            if index == last {
                continue;
            }
            let bucket = &mut self.groups[group];
            if bucket.last == END {
                bucket.first = index;
            } else {
                self.cells[bucket.last].next = index;
            }
            bucket.last = index;
        }
        let mut changed = false;
        for group in &self.groups {
            let mut index = group.first;
            while index != END {
                changed |= index != self.order.len();
                self.order.push(index);
                index = self.cells[index].next;
            }
        }
        self.order.push(last);
        if !changed {
            return false;
        }
        for group in &mut self.groups {
            group.lengths = group.style.0.map(color_bytes);
        }
        // Comparing command lengths is cheaper than serializing two candidates.
        // Photographic/high-cardinality backgrounds must not increase output.
        self.cost(self.order.iter().copied()) < self.cost(0..self.cells.len())
    }

    fn cost(&self, order: impl Iterator<Item = usize>) -> usize {
        let mut bytes = 0;
        let mut previous: Option<usize> = None;
        let mut style = PaintStyle::default();
        for index in order {
            let cell = &self.cells[index];
            if let Some(old) = previous.map(|i| &self.cells[i]) {
                if old.position.y == cell.position.y
                    && old.position.x.checked_add(1) == Some(cell.position.x)
                {
                } else if old.single_width
                    && old.position.y == cell.position.y
                    && cell.position.x > old.position.x.saturating_add(1)
                {
                    bytes += 3 + digits(usize::from(cell.position.x - old.position.x - 1));
                } else {
                    bytes += 4
                        + digits(usize::from(cell.position.x) + 1)
                        + digits(usize::from(cell.position.y) + 1);
                }
            } else {
                bytes += 4
                    + digits(usize::from(cell.position.x) + 1)
                    + digits(usize::from(cell.position.y) + 1);
            }
            let group = &self.groups[cell.group];
            let fg = style.0[0] != group.style.0[0];
            let bg = style.0[1] != group.style.0[1];
            if fg || bg {
                bytes += 3
                    + usize::from(fg) * usize::from(group.lengths[0])
                    + usize::from(bg) * usize::from(group.lengths[1])
                    + usize::from(fg && bg);
            }
            if style.0[2] != group.style.0[2] {
                bytes += 3 + usize::from(group.lengths[2]);
            }
            style = group.style;
            previous = Some(index);
        }
        // Glyph bytes and final resets are identical in either order.
        bytes
    }
}

fn draw_plain<'a, W: Write>(
    backend: &mut CrosstermBackend<W>,
    cells: impl Iterator<Item = CellRef<'a>>,
    short: bool,
) -> io::Result<()> {
    let (mut foreground, mut background, mut underline) =
        (Color::Reset, Color::Reset, Color::Reset);
    let mut previous: Option<(Position, bool)> = None;
    for draw in cells {
        let (x, y) = (draw.position.x, draw.position.y);
        let cell = draw.cell;
        if !matches!(previous,Some((p,_)) if p.x.checked_add(1)==Some(x)&&p.y==y) {
            if let Some((p, _)) = previous
                .filter(|(p, single)| short && *single && p.y == y && x > p.x.saturating_add(1))
            {
                // A valid target to the right proves the previous one-cell glyph
                // did not reach the physical right edge/wrap boundary.
                queue!(backend, MoveRight(x - p.x - 1))?;
            } else {
                queue!(backend, MoveTo(x, y))?;
            }
        }
        previous = Some((draw.position, draw.single_width));
        if cell.fg != foreground || cell.bg != background {
            queue!(
                backend,
                SetColors(Colors {
                    foreground: (cell.fg != foreground).then(|| cell.fg.into_crossterm()),
                    background: (cell.bg != background).then(|| cell.bg.into_crossterm()),
                })
            )?;
            foreground = cell.fg;
            background = cell.bg;
        }
        if cell.underline_color != underline {
            queue!(
                backend,
                SetUnderlineColor(cell.underline_color.into_crossterm())
            )?;
            underline = cell.underline_color;
        }
        queue!(backend, Print(cell.symbol()))?;
    }
    backend.draw(std::iter::empty())
}

fn encode_cells<'a, W: Write>(
    backend: &mut CrosstermBackend<W>,
    content: impl Iterator<Item = CellRef<'a>>,
    short: bool,
) -> io::Result<()> {
    if Colored::ansi_color_disabled_memoized() {
        return backend.draw(content.map(|d| (d.position.x, d.position.y, d.cell)));
    }
    let mut content = content.peekable();
    if content.peek().is_none() {
        return backend.draw(std::iter::empty());
    }
    while let Some(draw) = content.peek() {
        if draw.cell.modifier.is_empty() {
            draw_plain(
                backend,
                std::iter::from_fn(|| content.next_if(|d| d.cell.modifier.is_empty())),
                short,
            )?;
        } else {
            backend.draw(
                std::iter::from_fn(|| content.next_if(|d| !d.cell.modifier.is_empty()))
                    .map(|d| (d.position.x, d.position.y, d.cell)),
            )?;
        }
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct DrawBuffer {
    commands: Vec<u8>,
    frame: FramePlan,
}

impl DrawBuffer {
    pub(crate) fn draw<'a, W: Write, I>(
        &mut self,
        backend: &mut CrosstermBackend<W>,
        content: I,
    ) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Legacy Windows commands can execute WinAPI calls while queued: they
        // must not be staged into a byte buffer with delayed glyph writes.
        #[cfg(windows)]
        if !crossterm::ansi_support::supports_ansi() {
            return encode_cells(
                backend,
                content.map(|(x, y, cell)| CellRef {
                    position: Position::new(x, y),
                    cell,
                    single_width: false,
                }),
                false,
            );
        }
        self.frame.collect(content);
        let grouped = !Colored::ansi_color_disabled_memoized() && self.frame.prepare();
        self.commands.clear();
        let mut encoder = CrosstermBackend::new(&mut self.commands);
        if grouped {
            encode_cells(
                &mut encoder,
                self.frame
                    .order
                    .iter()
                    .map(|&i| self.frame.cells[i].as_ref()),
                true,
            )?;
        } else {
            encode_cells(
                &mut encoder,
                self.frame.cells.iter().map(PendingCell::as_ref),
                true,
            )?;
        }
        // Buffer ownership stays here; dropping after an I/O error cannot flush
        // stale commands after TerminalGuard has restored the user's terminal.
        backend.write_all(&self.commands)
    }
}

/// Buffered Crossterm output with lossless grouping and independent colors.
pub struct TerminalBackend<W: Write> {
    inner: CrosstermBackend<W>,
    buffer: DrawBuffer,
}

impl<W: Write> TerminalBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            buffer: DrawBuffer::default(),
        }
    }
}

impl<W: Write> Backend for TerminalBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.buffer.draw(&mut self.inner, content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn draw_batches_commands_in_one_write() {
        crossterm::style::force_color_output(true);
        #[derive(Default)]
        struct Writer {
            writes: usize,
            bytes: Vec<u8>,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer::default();
        let cells: Vec<_> = (0..32)
            .map(|i| {
                let mut cell = Cell::new("▀");
                cell.set_fg(Color::Rgb(130, 95, 210)).set_bg(Color::Rgb(
                    if i % 2 == 0 { 30 } else { 200 },
                    40,
                    50,
                ));
                cell
            })
            .collect();
        TerminalBackend::new(&mut writer)
            .draw(
                cells
                    .iter()
                    .enumerate()
                    .map(|(x, cell)| (x as u16, 0, cell)),
            )
            .unwrap();
        assert_eq!(writer.writes, 1);
        assert_eq!(
            TerminalState::read(&writer.bytes).painted.len(),
            cells.len()
        );
    }

    #[derive(Debug, PartialEq)]
    struct Paint {
        position: Position,
        glyph: String,
        foreground: Color,
        background: Color,
        underline: Color,
        modifier: Modifier,
    }

    #[derive(Debug, Default, PartialEq)]
    struct TerminalState {
        position: Position,
        foreground: Color,
        background: Color,
        underline: Color,
        modifier: Modifier,
        painted: Vec<Paint>,
    }

    impl TerminalState {
        fn read(bytes: &[u8]) -> Self {
            let mut state = Self {
                foreground: Color::Reset,
                background: Color::Reset,
                underline: Color::Reset,
                ..Self::default()
            };
            let mut text = std::str::from_utf8(bytes).unwrap();
            while !text.is_empty() {
                if let Some(sequence) = text.strip_prefix("\x1b[") {
                    let end = sequence.find(|ch: char| ('@'..='~').contains(&ch)).unwrap();
                    let values: Vec<u16> = sequence[..end]
                        .split(';')
                        .map(|value| value.parse().unwrap_or(0))
                        .collect();
                    match sequence.as_bytes()[end] {
                        b'H' => state.position = Position::new(values[1] - 1, values[0] - 1),
                        b'C' => state.position.x += values[0].max(1),
                        b'm' => state.sgr(&values),
                        command => panic!("Unexpected terminal command: {command}"),
                    }
                    text = &sequence[end + 1..];
                } else {
                    let glyph = text.graphemes(true).next().unwrap();
                    state.painted.push(Paint {
                        position: state.position,
                        glyph: glyph.into(),
                        foreground: state.foreground,
                        background: state.background,
                        underline: state.underline,
                        modifier: state.modifier,
                    });
                    state.position.x += glyph.width() as u16;
                    text = &text[glyph.len()..];
                }
            }
            state
        }

        fn sgr(&mut self, values: &[u16]) {
            let mut values = values.iter().copied();
            while let Some(value) = values.next() {
                match value {
                    0 => {
                        self.foreground = Color::Reset;
                        self.background = Color::Reset;
                        self.underline = Color::Reset;
                        self.modifier = Modifier::empty();
                    }
                    1 => self.modifier.insert(Modifier::BOLD),
                    2 => self.modifier.insert(Modifier::DIM),
                    3 => self.modifier.insert(Modifier::ITALIC),
                    4 => self.modifier.insert(Modifier::UNDERLINED),
                    5 => self.modifier.insert(Modifier::SLOW_BLINK),
                    6 => self.modifier.insert(Modifier::RAPID_BLINK),
                    7 => self.modifier.insert(Modifier::REVERSED),
                    8 => self.modifier.insert(Modifier::HIDDEN),
                    9 => self.modifier.insert(Modifier::CROSSED_OUT),
                    22 => self.modifier.remove(Modifier::BOLD | Modifier::DIM),
                    23 => self.modifier.remove(Modifier::ITALIC),
                    24 => self.modifier.remove(Modifier::UNDERLINED),
                    25 => self
                        .modifier
                        .remove(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
                    27 => self.modifier.remove(Modifier::REVERSED),
                    28 => self.modifier.remove(Modifier::HIDDEN),
                    29 => self.modifier.remove(Modifier::CROSSED_OUT),
                    30..=37 => self.foreground = Color::Indexed((value - 30) as u8),
                    39 => self.foreground = Color::Reset,
                    40..=47 => self.background = Color::Indexed((value - 40) as u8),
                    49 => self.background = Color::Reset,
                    59 => self.underline = Color::Reset,
                    90..=97 => self.foreground = Color::Indexed((value - 90 + 8) as u8),
                    100..=107 => self.background = Color::Indexed((value - 100 + 8) as u8),
                    38 | 48 | 58 => {
                        let color = match values.next().unwrap() {
                            2 => Color::Rgb(
                                values.next().unwrap() as u8,
                                values.next().unwrap() as u8,
                                values.next().unwrap() as u8,
                            ),
                            5 => Color::Indexed(values.next().unwrap() as u8),
                            mode => panic!("Unexpected color mode: {mode}"),
                        };
                        match value {
                            38 => self.foreground = color,
                            48 => self.background = color,
                            _ => self.underline = color,
                        }
                    }
                    code => panic!("Unexpected terminal attribute: {code}"),
                }
            }
        }
    }

    #[test]
    fn background_changes_do_not_repeat_the_foreground() {
        crossterm::style::force_color_output(true);
        let mut bytes = Vec::new();
        let cells: Vec<_> = (0..20)
            .map(|index| {
                let mut cell = Cell::new("▀");
                cell.set_fg(Color::Rgb(130, 95, 210))
                    .set_bg(Color::Rgb(index, 30, 40));
                cell
            })
            .collect();
        TerminalBackend::new(&mut bytes)
            .draw(
                cells
                    .iter()
                    .enumerate()
                    .map(|(x, cell)| (x as u16, 1, cell)),
            )
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("38;2;130;95;210").count(), 1);
        assert_eq!(text.matches("48;2;").count(), cells.len());
    }

    #[test]
    fn grouping_preserves_pixels_and_keeps_the_original_last_cell_last() {
        crossterm::style::force_color_output(true);
        let mut original = Vec::new();
        let mut backend = TerminalBackend::new(io::sink());
        for frame in 0..5 {
            original.clear();
            let cells: Vec<_> = (0..48)
                .map(|i| {
                    let mut cell = Cell::new(["▀", "⠿", "e\u{301}", " "][i % 4]);
                    cell.set_fg(Color::Rgb(130, 95, 210)).set_bg(Color::Rgb(
                        if (i + frame) % 2 == 0 { 30 } else { 200 },
                        40,
                        50,
                    ));
                    ((i % 16) as u16, (i / 16) as u16, cell)
                })
                .collect();
            let content = || cells.iter().map(|(x, y, cell)| (*x, *y, cell));
            CrosstermBackend::new(&mut original)
                .draw(content())
                .unwrap();
            backend.draw(content()).unwrap();
            let reference = TerminalState::read(&original);
            let mut actual = TerminalState::read(&backend.buffer.commands);
            assert_eq!(actual.painted.last(), reference.painted.last());
            assert_ne!(
                actual
                    .painted
                    .iter()
                    .map(|p| p.position)
                    .collect::<Vec<_>>(),
                reference
                    .painted
                    .iter()
                    .map(|p| p.position)
                    .collect::<Vec<_>>()
            );
            actual.painted.sort_by_key(|p| (p.position.y, p.position.x));
            let mut reference = reference;
            reference
                .painted
                .sort_by_key(|p| (p.position.y, p.position.x));
            assert_eq!(actual, reference);
        }
    }

    #[test]
    fn grouping_declines_a_plan_with_more_cursor_bytes() {
        let cells: Vec<_> = (0..8)
            .map(|i| {
                let mut cell = Cell::new("x");
                if i % 2 != 0 {
                    cell.set_bg(Color::Indexed(0));
                }
                (1000 + (i % 2) as u16, 1000 + (i / 2) as u16, cell)
            })
            .collect();
        let mut frame = FramePlan::default();
        frame.collect(cells.iter().map(|(x, y, cell)| (*x, *y, cell)));
        assert!(!frame.prepare());
        assert!(frame.cost(frame.order.iter().copied()) > frame.cost(0..frame.cells.len()));
    }

    #[test]
    fn wide_overlapping_and_unsorted_cells_keep_the_original_paint_order() {
        crossterm::style::force_color_output(true);
        for case in 0..3 {
            let cells: Vec<_> = (0..32)
                .map(|i| {
                    let mut cell = Cell::new(if case == 0 && i % 3 == 0 {
                        "界"
                    } else {
                        "▀"
                    });
                    cell.set_bg(Color::Rgb(if i % 2 == 0 { 30 } else { 200 }, 40, 50));
                    let x = match case {
                        1 => i / 2 * 3, // Repeated coordinates must not be reordered.
                        2 => 99 - i * 3,
                        _ => i * 3,
                    };
                    (x, 0, cell)
                })
                .collect();
            let content = || cells.iter().map(|(x, y, cell)| (*x, *y, cell));
            let mut original = Vec::new();
            let mut optimized = Vec::new();
            CrosstermBackend::new(&mut original)
                .draw(content())
                .unwrap();
            TerminalBackend::new(&mut optimized)
                .draw(content())
                .unwrap();
            assert_eq!(
                TerminalState::read(&optimized),
                TerminalState::read(&original)
            );
        }
    }

    #[test]
    fn grouping_cost_matches_encoding_and_never_increases_plain_frame_output() {
        crossterm::style::force_color_output(true);
        let colors = [
            Color::Reset,
            Color::Black,
            Color::Indexed(0),
            Color::Indexed(123),
            Color::Rgb(1, 22, 233),
            Color::Rgb(130, 95, 210),
        ];
        let mut backend = TerminalBackend::new(io::sink());
        for phase in 0..30 {
            let cells: Vec<_> = (0..64)
                .map(|i| {
                    let mut cell = Cell::new(["▀", "e\u{301}", "⠿", " "][i % 4]);
                    cell.set_fg(colors[(i / 3 + phase) % colors.len()])
                        .set_bg(colors[(i + phase) % colors.len()]);
                    cell.underline_color = colors[(i / 7 + phase) % colors.len()];
                    ((i % 16 * 3 + 1) as u16, (i / 16 + phase) as u16, cell)
                })
                .collect();
            let content = || cells.iter().map(|(x, y, cell)| (*x, *y, cell));
            let mut frame = FramePlan::default();
            frame.collect(content());
            frame.prepare();
            let mut original = Vec::new();
            encode_cells(
                &mut CrosstermBackend::new(&mut original),
                frame.cells.iter().map(PendingCell::as_ref),
                true,
            )
            .unwrap();
            let mut grouped = Vec::new();
            encode_cells(
                &mut CrosstermBackend::new(&mut grouped),
                frame.order.iter().map(|&i| frame.cells[i].as_ref()),
                true,
            )
            .unwrap();
            let mut resets = Vec::new();
            CrosstermBackend::new(&mut resets)
                .draw(std::iter::empty())
                .unwrap();
            let common = resets.len()
                + frame
                    .cells
                    .iter()
                    .map(|draw| draw.cell.symbol().len())
                    .sum::<usize>();
            assert_eq!(frame.cost(0..frame.cells.len()) + common, original.len());
            assert_eq!(
                frame.cost(frame.order.iter().copied()) + common,
                grouped.len()
            );
            backend.draw(content()).unwrap();
            assert!(backend.buffer.commands.len() <= original.len());
        }
    }

    #[test]
    fn io_failure_does_not_flush_pending_commands_when_dropped() {
        struct Fails {
            bytes: Vec<u8>,
        }
        impl Write for Fails {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.bytes.len() >= 7 {
                    return Err(io::Error::other("fixture"));
                }
                let count = (7 - self.bytes.len()).min(bytes.len());
                self.bytes.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                panic!("Unexpected flush after draw failure")
            }
        }
        let mut writer = Fails { bytes: Vec::new() };
        let cell = Cell::new("abc");
        {
            let mut backend = TerminalBackend::new(&mut writer);
            assert!(backend.draw(std::iter::once((1, 1, &cell))).is_err());
        }
        assert_eq!(writer.bytes.len(), 7);
    }

    #[test]
    fn no_color_keeps_upstream_modifier_behavior() {
        // Crossterm's color switch is global; use a separate test process.
        if std::env::var_os("PLAVNO_NO_COLOR_TEST").is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "terminal_backend::tests::no_color_keeps_upstream_modifier_behavior",
                    "--nocapture",
                ])
                .env("PLAVNO_NO_COLOR_TEST", "1")
                .env("NO_COLOR", "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let mut plain = Cell::new("a");
        plain.set_fg(Color::Blue);
        let mut bold = plain.clone();
        bold.set_symbol("b");
        bold.modifier = Modifier::BOLD;
        let cells = [plain, bold];
        let content = || {
            cells
                .iter()
                .enumerate()
                .map(|(x, cell)| (x as u16, 0, cell))
        };
        let mut original = Vec::new();
        let mut optimized = Vec::new();
        CrosstermBackend::new(&mut original)
            .draw(content())
            .unwrap();
        TerminalBackend::new(&mut optimized)
            .draw(content())
            .unwrap();
        assert_eq!(
            TerminalState::read(&optimized),
            TerminalState::read(&original)
        );
    }

    #[test]
    fn colors_styles_wide_glyphs_and_redraws_match_the_upstream_backend() {
        crossterm::style::force_color_output(true);
        let colors = [
            Color::Rgb(50, 80, 110),
            Color::Indexed(123),
            Color::Red,
            Color::Reset,
        ];
        let modifiers = [
            Modifier::empty(),
            Modifier::BOLD,
            Modifier::BOLD | Modifier::DIM,
            Modifier::DIM,
            Modifier::empty(),
            Modifier::ITALIC | Modifier::UNDERLINED,
            Modifier::SLOW_BLINK | Modifier::REVERSED,
            Modifier::RAPID_BLINK | Modifier::HIDDEN | Modifier::CROSSED_OUT,
        ];
        let mut original = Vec::new();
        let mut optimized = Vec::new();
        for frame in 0..4 {
            let cells: Vec<_> = (0..32)
                .map(|index| {
                    let symbols = if frame % 2 == 0 {
                        ["▀", "界", "e\u{301}", "👩‍💻", " "]
                    } else {
                        ["▀", "x", "e\u{301}", "⠿", " "]
                    };
                    let mut cell = Cell::new(symbols[index % 5]);
                    cell.set_fg(colors[(index / 4 + frame) % colors.len()])
                        .set_bg(colors[(index + frame) % colors.len()]);
                    cell.modifier = modifiers[(index + frame) % modifiers.len()];
                    cell.underline_color = colors[(index / 7 + frame) % colors.len()];
                    (
                        (index % 10 * 6 + 2) as u16,
                        (index / 10 * 2 + 1) as u16,
                        cell,
                    )
                })
                .collect();
            let content = || cells.iter().map(|(x, y, cell)| (*x, *y, cell));
            CrosstermBackend::new(&mut original)
                .draw(content())
                .unwrap();
            TerminalBackend::new(&mut optimized)
                .draw(content())
                .unwrap();
            CrosstermBackend::new(&mut original)
                .draw(std::iter::empty())
                .unwrap();
            TerminalBackend::new(&mut optimized)
                .draw(std::iter::empty())
                .unwrap();
            assert_eq!(
                TerminalState::read(&optimized),
                TerminalState::read(&original)
            );
        }
    }
}
