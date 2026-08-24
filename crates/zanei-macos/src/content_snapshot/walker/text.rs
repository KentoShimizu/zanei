//! UTF-8-safe, blank-line-normalizing text assembly.

pub(super) struct TextAssembler {
    text: String,
    max_bytes: usize,
    previous_empty: bool,
}

impl TextAssembler {
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
            previous_empty: false,
        }
    }

    pub(super) fn push(&mut self, fragment: &str) -> bool {
        let fragment = fragment.trim();
        if fragment.is_empty() {
            return true;
        }
        for line in fragment.lines() {
            let line = line.trim_end_matches('\r');
            let empty = line.trim().is_empty();
            if empty && self.previous_empty {
                continue;
            }
            if !self.text.is_empty() && !self.append("\n") {
                return false;
            }
            if !empty && !self.append(line) {
                return false;
            }
            self.previous_empty = empty;
        }
        true
    }

    fn append(&mut self, value: &str) -> bool {
        let remaining = self.max_bytes.saturating_sub(self.text.len());
        if value.len() <= remaining {
            self.text.push_str(value);
            return true;
        }
        let mut boundary = remaining.min(value.len());
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        self.text.push_str(&value[..boundary]);
        false
    }

    pub(super) fn finish(mut self) -> String {
        self.text.truncate(self.text.trim_end().len());
        self.text
    }
}
