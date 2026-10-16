pub fn screen_text(bytes: &[u8], width: usize, height: usize) -> String {
    let mut cells = vec![vec![' '; width]; height];
    let (mut x, mut y) = (0usize, 0usize);
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    for end in chars.by_ref() {
                        if ('@'..='~').contains(&end) {
                            let values: Vec<usize> =
                                params.split(';').map(|v| v.parse().unwrap_or(0)).collect();
                            let first = values[0];
                            match end {
                                'H' | 'f' => {
                                    y = first.max(1) - 1;
                                    x = values.get(1).copied().unwrap_or(1).max(1) - 1;
                                }
                                'G' => x = first.max(1) - 1,
                                'd' => y = first.max(1) - 1,
                                'A' => y = y.saturating_sub(first.max(1)),
                                'B' => y += first.max(1),
                                'C' => x += first.max(1),
                                'D' => x = x.saturating_sub(first.max(1)),
                                'J' if first == 2 => cells.iter_mut().for_each(|row| row.fill(' ')),
                                'K' if y < cells.len() => {
                                    let range = match first {
                                        1 => 0..(x + 1).min(width),
                                        2 => 0..width,
                                        _ => x.min(width)..width,
                                    };
                                    cells[y][range].fill(' ');
                                }
                                'X' if y < cells.len() => {
                                    cells[y]
                                        [x.min(width)..x.saturating_add(first.max(1)).min(width)]
                                        .fill(' ');
                                }
                                _ => {}
                            }
                            break;
                        }
                        params.push(end);
                    }
                }
                Some(']' | 'P') => {
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' || ch == '\x1b' && chars.next() == Some('\\') {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => x = 0,
            '\n' => y += 1,
            '\x08' => x = x.saturating_sub(1),
            ch if !ch.is_control() => {
                if y < cells.len() && x < cells[y].len() {
                    cells[y][x] = ch;
                }
                x += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
            _ => {}
        }
    }
    cells
        .into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
