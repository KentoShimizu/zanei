pub(super) fn sanitize_human_text(value: &str) -> String {
    value
        .chars()
        .fold(String::new(), |mut sanitized, character| {
            if character.is_control() {
                sanitized.extend(character.escape_default());
            } else {
                sanitized.push(character);
            }
            sanitized
        })
}

#[cfg(test)]
mod tests {
    use super::sanitize_human_text;

    #[test]
    fn controls_are_visible_and_printable_unicode_is_unchanged() {
        assert_eq!(
            sanitize_human_text("chrome\nforged\r\u{1b}[2J"),
            "chrome\\nforged\\r\\u{1b}[2J"
        );
        assert_eq!(
            sanitize_human_text("failed\r\n\u{1b}[31m"),
            "failed\\r\\n\\u{1b}[31m"
        );
        assert_eq!(sanitize_human_text("Chrome: 利用不可"), "Chrome: 利用不可");
    }
}
