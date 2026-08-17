//! Pure translation from native input observations to core event payloads.

use zanei_core::schema::{
    ClipboardCopyData, ClipboardOrigin, ClipboardPasteData, ContentKind, FieldKind, InputKeyData,
    InputKeyKind, InputScrollData, Modifier, ScrollDirection,
};

const KEYCODE_V: u16 = 0x09;
const KEYCODE_C: u16 = 0x08;
const KEYCODE_RETURN: u16 = 0x24;
const KEYCODE_TAB: u16 = 0x30;
const KEYCODE_SPACE: u16 = 0x31;
const KEYCODE_DELETE: u16 = 0x33;
const KEYCODE_ESCAPE: u16 = 0x35;
const KEYCODE_HOME: u16 = 0x73;
const KEYCODE_PAGE_UP: u16 = 0x74;
const KEYCODE_FORWARD_DELETE: u16 = 0x75;
const KEYCODE_END: u16 = 0x77;
const KEYCODE_PAGE_DOWN: u16 = 0x79;
const KEYCODE_LEFT: u16 = 0x7b;
const KEYCODE_RIGHT: u16 = 0x7c;
const KEYCODE_DOWN: u16 = 0x7d;
const KEYCODE_UP: u16 = 0x7e;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct KeyModifiers {
    pub(crate) cmd: bool,
    pub(crate) shift: bool,
    pub(crate) opt: bool,
    pub(crate) ctrl: bool,
    pub(crate) function: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeyObservation {
    pub(crate) key_code: u16,
    pub(crate) modifiers: KeyModifiers,
    pub(crate) text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MouseObservation {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) button: u32,
    pub(crate) click_count: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClickData {
    pub(crate) pid: i32,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) button: u32,
    pub(crate) click_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PasteboardKind {
    Text,
    Image,
    File,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PasteboardContent {
    pub(crate) kind: PasteboardKind,
    pub(crate) size_bytes: Option<u64>,
    pub(crate) text: Option<String>,
}

pub(crate) fn key_data(
    observation: &KeyObservation,
    text_content: bool,
    field_kind: Option<FieldKind>,
    ime_active: bool,
) -> InputKeyData {
    let modifiers = ordered_modifiers(observation.modifiers);
    let kind = classify_key(observation);
    let combo = (kind == InputKeyKind::Shortcut).then(|| shortcut_combo(observation, &modifiers));
    let text = (text_content && !ime_active && kind == InputKeyKind::Text)
        .then(|| observation.text.clone())
        .flatten();
    InputKeyData {
        kind,
        modifiers,
        count: 1,
        combo,
        text,
        field_kind,
    }
}

pub(crate) fn is_paste_shortcut(observation: &KeyObservation) -> bool {
    observation.key_code == KEYCODE_V && observation.modifiers.cmd
}

pub(crate) fn is_copy_shortcut(observation: &KeyObservation) -> bool {
    observation.key_code == KEYCODE_C && observation.modifiers.cmd
}

pub(crate) fn click_data(pid: i64, observation: MouseObservation) -> Option<ClickData> {
    let pid = i32::try_from(pid).ok()?;
    let click_count = u64::try_from(observation.click_count).ok()?;
    if click_count == 0 || !observation.x.is_finite() || !observation.y.is_finite() {
        return None;
    }
    Some(ClickData {
        pid,
        x: observation.x,
        y: observation.y,
        button: observation.button,
        click_count,
    })
}

pub(crate) fn scroll_data(vertical: f64, horizontal: f64) -> Option<InputScrollData> {
    if !vertical.is_finite() || !horizontal.is_finite() {
        return None;
    }
    let (direction, amount) = if vertical.abs() >= horizontal.abs() && vertical != 0.0 {
        (
            if vertical > 0.0 {
                ScrollDirection::Up
            } else {
                ScrollDirection::Down
            },
            vertical.abs(),
        )
    } else if horizontal != 0.0 {
        (
            if horizontal > 0.0 {
                ScrollDirection::Left
            } else {
                ScrollDirection::Right
            },
            horizontal.abs(),
        )
    } else {
        return None;
    };
    Some(InputScrollData {
        direction,
        amount,
        count: 1,
    })
}

pub(crate) fn clipboard_copy(
    content: PasteboardContent,
    origin: ClipboardOrigin,
) -> ClipboardCopyData {
    ClipboardCopyData {
        origin,
        content_kind: content_kind(content.kind),
        size_bytes: content.size_bytes,
        text: content.text,
    }
}

pub(crate) fn clipboard_paste(
    content: PasteboardContent,
    field_kind: Option<FieldKind>,
) -> ClipboardPasteData {
    ClipboardPasteData {
        content_kind: content_kind(content.kind),
        size_bytes: content.size_bytes,
        text: content.text,
        field_kind,
    }
}

fn classify_key(observation: &KeyObservation) -> InputKeyKind {
    if observation.modifiers.cmd || observation.modifiers.ctrl {
        InputKeyKind::Shortcut
    } else if matches!(
        observation.key_code,
        KEYCODE_TAB
            | KEYCODE_HOME
            | KEYCODE_PAGE_UP
            | KEYCODE_END
            | KEYCODE_PAGE_DOWN
            | KEYCODE_LEFT
            | KEYCODE_RIGHT
            | KEYCODE_DOWN
            | KEYCODE_UP
    ) {
        InputKeyKind::Navigation
    } else if matches!(
        observation.key_code,
        KEYCODE_DELETE | KEYCODE_FORWARD_DELETE
    ) {
        InputKeyKind::Delete
    } else if is_text_keycode(observation.key_code) {
        InputKeyKind::Text
    } else {
        InputKeyKind::Other
    }
}

fn ordered_modifiers(value: KeyModifiers) -> Vec<Modifier> {
    [
        (value.cmd, Modifier::Cmd),
        (value.shift, Modifier::Shift),
        (value.opt, Modifier::Opt),
        (value.ctrl, Modifier::Ctrl),
        (value.function, Modifier::Fn),
    ]
    .into_iter()
    .filter_map(|(present, modifier)| present.then_some(modifier))
    .collect()
}

fn shortcut_combo(observation: &KeyObservation, modifiers: &[Modifier]) -> String {
    let mut parts: Vec<&str> = modifiers
        .iter()
        .map(|modifier| match modifier {
            Modifier::Cmd => "cmd",
            Modifier::Shift => "shift",
            Modifier::Opt => "opt",
            Modifier::Ctrl => "ctrl",
            Modifier::Fn => "fn",
        })
        .collect();
    let key = key_name(observation);
    parts.push(&key);
    parts.join("+")
}

fn key_name(observation: &KeyObservation) -> String {
    match observation.key_code {
        0x00 => "a".to_owned(),
        0x01 => "s".to_owned(),
        0x02 => "d".to_owned(),
        0x03 => "f".to_owned(),
        0x04 => "h".to_owned(),
        0x05 => "g".to_owned(),
        0x06 => "z".to_owned(),
        0x07 => "x".to_owned(),
        0x08 => "c".to_owned(),
        0x09 => "v".to_owned(),
        0x0b => "b".to_owned(),
        0x0c => "q".to_owned(),
        0x0d => "w".to_owned(),
        0x0e => "e".to_owned(),
        0x0f => "r".to_owned(),
        0x10 => "y".to_owned(),
        0x11 => "t".to_owned(),
        0x1f => "o".to_owned(),
        0x20 => "u".to_owned(),
        0x22 => "i".to_owned(),
        0x23 => "p".to_owned(),
        0x25 => "l".to_owned(),
        0x26 => "j".to_owned(),
        0x28 => "k".to_owned(),
        0x2d => "n".to_owned(),
        0x2e => "m".to_owned(),
        KEYCODE_RETURN => "return".to_owned(),
        KEYCODE_TAB => "tab".to_owned(),
        KEYCODE_SPACE => "space".to_owned(),
        KEYCODE_DELETE => "delete".to_owned(),
        KEYCODE_ESCAPE => "escape".to_owned(),
        KEYCODE_HOME => "home".to_owned(),
        KEYCODE_PAGE_UP => "page_up".to_owned(),
        KEYCODE_FORWARD_DELETE => "forward_delete".to_owned(),
        KEYCODE_END => "end".to_owned(),
        KEYCODE_PAGE_DOWN => "page_down".to_owned(),
        KEYCODE_LEFT => "left".to_owned(),
        KEYCODE_RIGHT => "right".to_owned(),
        KEYCODE_DOWN => "down".to_owned(),
        KEYCODE_UP => "up".to_owned(),
        key_code => format!("key_{key_code:02x}"),
    }
}

const fn is_text_keycode(key_code: u16) -> bool {
    matches!(
        key_code,
        0x00..=0x23
            | 0x25..=0x2f
            | KEYCODE_SPACE
            | 0x32
            | 0x41
            | 0x43
            | 0x45
            | 0x4b
            | 0x4e
            | 0x51..=0x59
            | 0x5b..=0x5f
    )
}

const fn content_kind(kind: PasteboardKind) -> ContentKind {
    match kind {
        PasteboardKind::Text => ContentKind::Text,
        PasteboardKind::Image => ContentKind::Image,
        PasteboardKind::File => ContentKind::File,
        PasteboardKind::Other => ContentKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key_code: u16, text: Option<&str>, modifiers: KeyModifiers) -> KeyObservation {
        KeyObservation {
            key_code,
            modifiers,
            text: text.map(str::to_owned),
        }
    }

    #[test]
    fn classifies_keys_and_gates_text_content() {
        let printable = key(0, None, KeyModifiers::default());
        assert_eq!(
            key_data(&printable, false, None, false).kind,
            InputKeyKind::Text
        );
        assert_eq!(key_data(&printable, false, None, false).text, None);
        let captured = key(0, Some("a"), KeyModifiers::default());
        assert_eq!(
            key_data(&captured, true, Some(FieldKind::Text), false)
                .text
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            key_data(
                &key(KEYCODE_LEFT, None, KeyModifiers::default()),
                false,
                None,
                false,
            )
            .kind,
            InputKeyKind::Navigation
        );
        assert_eq!(
            key_data(
                &key(KEYCODE_DELETE, None, KeyModifiers::default()),
                false,
                None,
                false,
            )
            .kind,
            InputKeyKind::Delete
        );
        assert_eq!(
            key_data(
                &key(KEYCODE_ESCAPE, None, KeyModifiers::default()),
                false,
                None,
                false,
            )
            .kind,
            InputKeyKind::Other
        );
    }

    #[test]
    fn shortcut_has_ordered_modifiers_and_combo_only() {
        let observation = key(
            KEYCODE_V,
            None,
            KeyModifiers {
                cmd: true,
                shift: true,
                opt: true,
                ctrl: true,
                function: true,
            },
        );
        let data = key_data(&observation, true, None, false);
        assert_eq!(data.kind, InputKeyKind::Shortcut);
        assert_eq!(
            data.modifiers,
            [
                Modifier::Cmd,
                Modifier::Shift,
                Modifier::Opt,
                Modifier::Ctrl,
                Modifier::Fn
            ]
        );
        assert_eq!(data.combo.as_deref(), Some("cmd+shift+opt+ctrl+fn+v"));
        assert_eq!(data.text, None);
        assert!(is_paste_shortcut(&observation));
    }

    #[test]
    fn ime_suppresses_only_direct_key_text() {
        let observation = key(0, Some("a"), KeyModifiers::default());
        let direct = key_data(&observation, true, Some(FieldKind::Text), false);
        let ime = key_data(&observation, true, Some(FieldKind::Text), true);

        assert_eq!(direct.text.as_deref(), Some("a"));
        assert_eq!(ime.text, None);
        assert_eq!(ime.kind, direct.kind);
        assert_eq!(ime.count, direct.count);
        assert_eq!(ime.combo, direct.combo);
        assert_eq!(ime.field_kind, direct.field_kind);
    }

    #[test]
    fn mouse_down_keeps_pid_and_emits_one_click_without_another_event() {
        let click = click_data(
            501,
            MouseObservation {
                x: 12.5,
                y: 24.0,
                button: 0,
                click_count: 1,
            },
        )
        .expect("valid click");
        assert_eq!(click.pid, 501);
        assert_eq!(click.x, 12.5);
        assert_eq!(click.y, 24.0);
        assert_eq!(click.click_count, 1);
    }

    #[test]
    fn scroll_uses_dominant_axis_and_rejects_invalid_values() {
        assert_eq!(scroll_data(0.0, 0.0), None);
        assert_eq!(scroll_data(f64::NAN, 1.0), None);
        assert_eq!(
            scroll_data(-4.0, 2.0).map(|data| data.direction),
            Some(ScrollDirection::Down)
        );
        assert_eq!(
            scroll_data(1.0, -3.0).map(|data| data.direction),
            Some(ScrollDirection::Right)
        );
        assert_eq!(scroll_data(1.0, -3.0).map(|data| data.amount), Some(3.0));
    }

    #[test]
    fn clipboard_payload_preserves_privacy_gated_values() {
        let content = PasteboardContent {
            kind: PasteboardKind::Text,
            size_bytes: Some(3),
            text: Some("abc".to_owned()),
        };
        let copy = clipboard_copy(content.clone(), ClipboardOrigin::CopyShortcut);
        let paste = clipboard_paste(content, Some(FieldKind::Search));
        assert_eq!(copy.content_kind, ContentKind::Text);
        assert_eq!(copy.size_bytes, Some(3));
        assert_eq!(paste.text.as_deref(), Some("abc"));
        assert_eq!(paste.field_kind, Some(FieldKind::Search));
    }
}
