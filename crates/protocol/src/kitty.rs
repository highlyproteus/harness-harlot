//! Kitty graphics Unicode placeholders, shared by the service (which stores
//! transmitted images) and the desktop (which paints them).
//!
//! An application displays a transmitted image by writing ordinary text cells:
//! U+10EEEE followed by combining diacritics naming the cell's row and column
//! within the image, with the image id in the cell's 24-bit foreground color.
//! Because the cells are text, they scroll, reflow, and survive tmux like any
//! other output. See kitty's `graphics-protocol.rst`, "Unicode placeholders".

/// The placeholder base character.
pub const KITTY_PLACEHOLDER: char = '\u{10eeee}';

/// Row/column diacritics: index `i` names row or column `i`. From kitty's
/// `rowcolumn-diacritics.txt`.
const ROW_COLUMN_DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30d}',
    '\u{30e}',
    '\u{310}',
    '\u{312}',
    '\u{33d}',
    '\u{33e}',
    '\u{33f}',
    '\u{346}',
    '\u{34a}',
    '\u{34b}',
    '\u{34c}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35b}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36a}',
    '\u{36b}',
    '\u{36c}',
    '\u{36d}',
    '\u{36e}',
    '\u{36f}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59c}',
    '\u{59d}',
    '\u{59e}',
    '\u{59f}',
    '\u{5a0}',
    '\u{5a1}',
    '\u{5a8}',
    '\u{5a9}',
    '\u{5ab}',
    '\u{5ac}',
    '\u{5af}',
    '\u{5c4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65a}',
    '\u{65b}',
    '\u{65d}',
    '\u{65e}',
    '\u{6d6}',
    '\u{6d7}',
    '\u{6d8}',
    '\u{6d9}',
    '\u{6da}',
    '\u{6db}',
    '\u{6dc}',
    '\u{6df}',
    '\u{6e0}',
    '\u{6e1}',
    '\u{6e2}',
    '\u{6e4}',
    '\u{6e7}',
    '\u{6e8}',
    '\u{6eb}',
    '\u{6ec}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73a}',
    '\u{73d}',
    '\u{73f}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74a}',
    '\u{7eb}',
    '\u{7ec}',
    '\u{7ed}',
    '\u{7ee}',
    '\u{7ef}',
    '\u{7f0}',
    '\u{7f1}',
    '\u{7f3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81b}',
    '\u{81c}',
    '\u{81d}',
    '\u{81e}',
    '\u{81f}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82a}',
    '\u{82b}',
    '\u{82c}',
    '\u{82d}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{f82}',
    '\u{f83}',
    '\u{f86}',
    '\u{f87}',
    '\u{135d}',
    '\u{135e}',
    '\u{135f}',
    '\u{17dd}',
    '\u{193a}',
    '\u{1a17}',
    '\u{1a75}',
    '\u{1a76}',
    '\u{1a77}',
    '\u{1a78}',
    '\u{1a79}',
    '\u{1a7a}',
    '\u{1a7b}',
    '\u{1a7c}',
    '\u{1b6b}',
    '\u{1b6d}',
    '\u{1b6e}',
    '\u{1b6f}',
    '\u{1b70}',
    '\u{1b71}',
    '\u{1b72}',
    '\u{1b73}',
    '\u{1cd0}',
    '\u{1cd1}',
    '\u{1cd2}',
    '\u{1cda}',
    '\u{1cdb}',
    '\u{1ce0}',
    '\u{1dc0}',
    '\u{1dc1}',
    '\u{1dc3}',
    '\u{1dc4}',
    '\u{1dc5}',
    '\u{1dc6}',
    '\u{1dc7}',
    '\u{1dc8}',
    '\u{1dc9}',
    '\u{1dcb}',
    '\u{1dcc}',
    '\u{1dd1}',
    '\u{1dd2}',
    '\u{1dd3}',
    '\u{1dd4}',
    '\u{1dd5}',
    '\u{1dd6}',
    '\u{1dd7}',
    '\u{1dd8}',
    '\u{1dd9}',
    '\u{1dda}',
    '\u{1ddb}',
    '\u{1ddc}',
    '\u{1ddd}',
    '\u{1dde}',
    '\u{1ddf}',
    '\u{1de0}',
    '\u{1de1}',
    '\u{1de2}',
    '\u{1de3}',
    '\u{1de4}',
    '\u{1de5}',
    '\u{1de6}',
    '\u{1dfe}',
    '\u{20d0}',
    '\u{20d1}',
    '\u{20d4}',
    '\u{20d5}',
    '\u{20d6}',
    '\u{20d7}',
    '\u{20db}',
    '\u{20dc}',
    '\u{20e1}',
    '\u{20e7}',
    '\u{20e9}',
    '\u{20f0}',
    '\u{2cef}',
    '\u{2cf0}',
    '\u{2cf1}',
    '\u{2de0}',
    '\u{2de1}',
    '\u{2de2}',
    '\u{2de3}',
    '\u{2de4}',
    '\u{2de5}',
    '\u{2de6}',
    '\u{2de7}',
    '\u{2de8}',
    '\u{2de9}',
    '\u{2dea}',
    '\u{2deb}',
    '\u{2dec}',
    '\u{2ded}',
    '\u{2dee}',
    '\u{2def}',
    '\u{2df0}',
    '\u{2df1}',
    '\u{2df2}',
    '\u{2df3}',
    '\u{2df4}',
    '\u{2df5}',
    '\u{2df6}',
    '\u{2df7}',
    '\u{2df8}',
    '\u{2df9}',
    '\u{2dfa}',
    '\u{2dfb}',
    '\u{2dfc}',
    '\u{2dfd}',
    '\u{2dfe}',
    '\u{2dff}',
    '\u{a66f}',
    '\u{a67c}',
    '\u{a67d}',
    '\u{a6f0}',
    '\u{a6f1}',
    '\u{a8e0}',
    '\u{a8e1}',
    '\u{a8e2}',
    '\u{a8e3}',
    '\u{a8e4}',
    '\u{a8e5}',
    '\u{a8e6}',
    '\u{a8e7}',
    '\u{a8e8}',
    '\u{a8e9}',
    '\u{a8ea}',
    '\u{a8eb}',
    '\u{a8ec}',
    '\u{a8ed}',
    '\u{a8ee}',
    '\u{a8ef}',
    '\u{a8f0}',
    '\u{a8f1}',
    '\u{aab0}',
    '\u{aab2}',
    '\u{aab3}',
    '\u{aab7}',
    '\u{aab8}',
    '\u{aabe}',
    '\u{aabf}',
    '\u{aac1}',
    '\u{fe20}',
    '\u{fe21}',
    '\u{fe22}',
    '\u{fe23}',
    '\u{fe24}',
    '\u{fe25}',
    '\u{fe26}',
    '\u{10a0f}',
    '\u{10a38}',
    '\u{1d185}',
    '\u{1d186}',
    '\u{1d187}',
    '\u{1d188}',
    '\u{1d189}',
    '\u{1d1aa}',
    '\u{1d1ab}',
    '\u{1d1ac}',
    '\u{1d1ad}',
    '\u{1d242}',
    '\u{1d243}',
    '\u{1d244}',
];

/// Row or column index named by a placeholder diacritic.
pub fn placeholder_diacritic_index(character: char) -> Option<u16> {
    ROW_COLUMN_DIACRITICS
        .binary_search(&character)
        .ok()
        .and_then(|index| u16::try_from(index).ok())
}

/// Diacritic naming row or column `index`, if the table covers it.
pub fn placeholder_diacritic(index: u16) -> Option<char> {
    ROW_COLUMN_DIACRITICS.get(usize::from(index)).copied()
}

/// Image id carried by a placeholder cell's 24-bit foreground color.
pub const fn placeholder_image_id(red: u8, green: u8, blue: u8) -> u32 {
    (red as u32) << 16 | (green as u32) << 8 | blue as u32
}

/// One terminal cell of a text run, classified as a placeholder or not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaceholderCell {
    /// Ordinary text.
    Text,
    /// Placeholder naming this cell's row and column in the image.
    Image { row: u16, column: u16 },
}

/// Splits run text into cells (a base character plus its combining marks)
/// and resolves each placeholder's row and column. A placeholder that omits
/// diacritics inherits from the placeholder to its left, as kitty specifies:
/// a missing row repeats the left row, and a missing column is left column + 1.
pub fn placeholder_cells(text: &str) -> Vec<PlaceholderCell> {
    let mut cells = Vec::new();
    let mut characters = text.chars().peekable();
    let mut previous: Option<(u16, u16)> = None;
    while let Some(base) = characters.next() {
        let mut marks = [None, None];
        let mut count = 0;
        while let Some(&next) = characters.peek() {
            let Some(index) = placeholder_diacritic_index(next) else {
                break;
            };
            if count < marks.len() {
                marks[count] = Some(index);
            }
            count += 1;
            characters.next();
        }
        if base != KITTY_PLACEHOLDER {
            previous = None;
            cells.push(PlaceholderCell::Text);
            continue;
        }
        let row = marks[0].or(previous.map(|(row, _)| row)).unwrap_or(0);
        let column = marks[1]
            .or_else(|| {
                previous
                    .filter(|(left_row, _)| *left_row == row)
                    .map(|(_, left_column)| left_column.saturating_add(1))
            })
            .unwrap_or(0);
        previous = Some((row, column));
        cells.push(PlaceholderCell::Image { row, column });
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: u16, column: u16) -> String {
        [
            KITTY_PLACEHOLDER,
            placeholder_diacritic(row).unwrap(),
            placeholder_diacritic(column).unwrap(),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn diacritic_table_is_sorted_so_lookup_round_trips_every_index() {
        for index in 0..297 {
            let mark = placeholder_diacritic(index).unwrap();
            assert_eq!(placeholder_diacritic_index(mark), Some(index));
        }
        assert_eq!(placeholder_diacritic(297), None);
    }

    #[test]
    fn explicit_row_and_column_marks_name_each_cell() {
        let text = format!("{}{}", cell(2, 7), cell(2, 8));
        assert_eq!(
            placeholder_cells(&text),
            vec![
                PlaceholderCell::Image { row: 2, column: 7 },
                PlaceholderCell::Image { row: 2, column: 8 },
            ]
        );
    }

    #[test]
    fn omitted_marks_inherit_row_and_advance_column_from_the_left() {
        let row_only: String = [KITTY_PLACEHOLDER, placeholder_diacritic(4).unwrap()]
            .into_iter()
            .collect();
        let text = format!("{}{row_only}{KITTY_PLACEHOLDER}", cell(4, 10));
        assert_eq!(
            placeholder_cells(&text),
            vec![
                PlaceholderCell::Image { row: 4, column: 10 },
                PlaceholderCell::Image { row: 4, column: 11 },
                PlaceholderCell::Image { row: 4, column: 12 },
            ]
        );
    }

    #[test]
    fn ordinary_text_breaks_inheritance_and_keeps_its_cell() {
        let text = format!("{}x{KITTY_PLACEHOLDER}", cell(1, 5));
        assert_eq!(
            placeholder_cells(&text),
            vec![
                PlaceholderCell::Image { row: 1, column: 5 },
                PlaceholderCell::Text,
                PlaceholderCell::Image { row: 0, column: 0 },
            ]
        );
    }

    #[test]
    fn image_id_is_the_24_bit_foreground_color() {
        assert_eq!(placeholder_image_id(0x12, 0x34, 0x56), 0x0012_3456);
    }
}
