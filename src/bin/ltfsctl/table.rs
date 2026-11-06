use unicode_width::UnicodeWidthStr;

fn cell(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Size columns by terminal cells, including their headers. Keep identifiers
/// intact and separate adjacent columns by two spaces rather than tab stops.
pub(super) fn render<const N: usize>(
    headers: [impl AsRef<str>; N],
    rows: &[[String; N]],
    right: &[usize],
    show_header: bool,
) -> String {
    let headers = headers.map(|header| cell(header.as_ref()));
    let rows: Vec<_> = rows
        .iter()
        .map(|row| row.each_ref().map(|v| cell(v)))
        .collect();
    let mut widths = headers.each_ref().map(|header| header.width());
    for row in &rows {
        for (width, value) in widths.iter_mut().zip(row) {
            *width = (*width).max(value.width());
        }
    }
    let mut output = String::new();
    for row in show_header.then_some(&headers).into_iter().chain(&rows) {
        for (column, value) in row.iter().enumerate() {
            if column > 0 {
                output.push_str("  ");
            }
            let padding = widths[column] - value.width();
            if right.contains(&column) {
                output.extend(std::iter::repeat_n(' ', padding));
            }
            output.push_str(value);
            if !right.contains(&column) && column + 1 < N {
                output.extend(std::iter::repeat_n(' ', padding));
            }
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_names_and_long_identifiers_keep_column_boundaries() {
        let id = "9893f492-6a55-4f09-8d39-742b345ac5e7";
        let rows = vec![
            ["归档池".into(), "7".into(), id.into()],
            ["e\u{301}👩‍💻".into(), "1200".into(), "短 ID".into()],
        ];
        let output = render(["Pool", "数量", "ID"], &rows, &[1], true);
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines[0], "Pool    数量  ID");
        assert_eq!(lines[1], format!("归档池     7  {id}"));
        assert_eq!(lines[2], "e\u{301}👩‍💻     1200  短 ID");
        assert!(!output.contains('\t'));
        assert!(lines.iter().all(|line| !line.ends_with(' ')));
    }

    #[test]
    fn missing_headers_and_control_characters_preserve_a_plain_table() {
        assert_eq!(render(["Name", "Value"], &[], &[], false), "");
        assert_eq!(render(["Name", "Value"], &[], &[], true), "Name  Value\n");
        let rows = [["A\tB\nC".into(), "数据".into()]];
        assert_eq!(
            render(["Name", "Value"], &rows, &[], false),
            "A B C  数据\n"
        );
    }
}
