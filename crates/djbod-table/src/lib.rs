//! Plain-text tables whose columns grow to fit every row, including the
//! header. Cells are single-line text; widths are terminal display columns,
//! so wide characters and combining marks receive the appropriate padding.

use std::fmt;

use unicode_width::UnicodeWidthStr;

/// A column's minimum width and alignment.
pub struct Column {
    width: usize,
    right: bool,
}

impl Column {
    pub fn left(min_width: usize) -> Self {
        Self {
            width: min_width,
            right: false,
        }
    }

    pub fn right(min_width: usize) -> Self {
        Self {
            width: min_width,
            right: true,
        }
    }
}

/// Collect rows before printing, so even the first row accounts for long
/// values later in the table. Columns have two spaces between them.
pub struct Table<const N: usize> {
    columns: [Column; N],
    rows: Vec<[String; N]>,
}

impl<const N: usize> Table<N> {
    pub fn new(columns: [Column; N]) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    /// A header, when present, is added in the same way as a data row.
    pub fn push(&mut self, row: [impl Into<String>; N]) {
        let row = row.map(Into::into);
        for i in 0..N {
            self.columns[i].width = self.columns[i].width.max(row[i].width());
        }
        self.rows.push(row);
    }
}

impl<const N: usize> fmt::Display for Table<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for row in &self.rows {
            for i in 0..N {
                if i > 0 {
                    f.write_str("  ")?;
                }
                let cell = &row[i];
                let column = &self.columns[i];
                let padding = column.width - cell.width();
                if column.right {
                    write!(f, "{:padding$}", "")?;
                }
                f.write_str(cell)?;
                if !column.right && i + 1 < N {
                    write!(f, "{:padding$}", "")?;
                }
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_long_values_expand_the_header_and_every_row() {
        let mut table = Table::new([Column::left(4), Column::right(3), Column::left(0)]);
        table.push(["NAME", "SIZE", "STATE"]);
        table.push(["-", "1", "active"]);
        table.push(["a-long-label", "12345", "draining"]);
        assert_eq!(
            table.to_string(),
            concat!(
                "NAME           SIZE  STATE\n",
                "-                 1  active\n",
                "a-long-label  12345  draining\n",
            )
        );
    }

    #[test]
    fn padding_uses_display_width_for_wide_combining_and_emoji_text() {
        let mut table = Table::new([Column::left(4), Column::left(0)]);
        table.push(["界界界", "wide"]);
        table.push(["e\u{301}", "combining"]);
        table.push(["👩‍💻", "emoji"]);
        table.push(["ascii", "plain"]);
        assert_eq!(
            table.to_string(),
            "界界界  wide\ne\u{301}       combining\n👩‍💻      emoji\nascii   plain\n"
        );
    }

    #[test]
    fn minimum_widths_and_header_only_tables_are_preserved() {
        let mut table = Table::new([Column::left(8), Column::right(6)]);
        assert_eq!(table.to_string(), "");
        table.push(["NAME", "SIZE"]);
        assert_eq!(table.to_string(), "NAME        SIZE\n");
        table.push(["disk", "42"]);
        assert_eq!(table.to_string(), "NAME        SIZE\ndisk          42\n");
    }

    #[test]
    fn the_last_text_column_has_no_trailing_padding() {
        let mut table = Table::new([Column::left(0), Column::left(20)]);
        table.push(["a", "short"]);
        table.push(["b", "a much longer final value"]);
        assert_eq!(
            table.to_string(),
            "a  short\nb  a much longer final value\n"
        );
    }
}
