use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::payload::deserialize_required_nullable;

/// Envelope `source` of a snapshot whose text was recognized from the window image.
/// A v4 `content.snapshot` carries `data.lines` exactly when it has this source.
pub const RECOGNIZED_TEXT_SOURCE: &str = "macos.ocr";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSnapshotTrigger {
    Settle,
    Refresh,
    FocusOut,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSnapshotCutoff {
    Time,
    Nodes,
    Bytes,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentSnapshotData {
    pub text: Option<String>,
    pub chars: u64,
    pub trigger: ContentSnapshotTrigger,
    shape: ContentSnapshotShape,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ContentSnapshotShape {
    /// A retained v2 payload.
    Legacy { complete: bool },
    /// A v4 payload without `lines`, or a retained v3 payload, which has the same fields.
    Current {
        cutoff: Option<ContentSnapshotCutoff>,
    },
    /// A v4 payload with `lines`; they are `None` only after the core size safety rule.
    Recognized {
        cutoff: Option<ContentSnapshotCutoff>,
        lines: Option<Vec<ContentSnapshotLine>>,
    },
}

/// One recognized line of a `macos.ocr` snapshot, in the same order as `text`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentSnapshotLine {
    pub text: String,
    pub x: WindowFraction,
    pub y: WindowFraction,
    pub w: WindowFraction,
    pub h: WindowFraction,
    pub bg: RgbColor,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub ink: Option<RgbColor>,
}

/// A position or size relative to the window, from 0 to 1, rounded to the documented
/// 4 decimal places. Keeping ten-thousandths makes off-grid values unrepresentable;
/// reading an off-grid number rounds it, as the published schema accepts any 0..=1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowFraction(u16);

const WINDOW_FRACTION_SCALE: f64 = 10_000.0;

impl WindowFraction {
    /// Rounds `ratio` to 4 decimal places. Returns `None` outside 0..=1, including NaN.
    #[must_use]
    pub fn from_ratio(ratio: f64) -> Option<Self> {
        // The range check bounds the rounded value to 0..=10_000, so the cast is exact.
        (0.0..=1.0)
            .contains(&ratio)
            .then(|| Self((ratio * WINDOW_FRACTION_SCALE).round() as u16))
    }

    #[must_use]
    pub fn ratio(self) -> f64 {
        f64::from(self.0) / WINDOW_FRACTION_SCALE
    }
}

impl Serialize for WindowFraction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.ratio())
    }
}

impl<'de> Deserialize<'de> for WindowFraction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_ratio(f64::deserialize(deserializer)?)
            .ok_or_else(|| de::Error::custom("window fraction must be from 0 to 1"))
    }
}

/// A colour serialized as `#RRGGBB` in uppercase hexadecimal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RgbColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Serialize for RgbColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&format_args!(
            "#{:02X}{:02X}{:02X}",
            self.red, self.green, self.blue
        ))
    }
}

impl<'de> Deserialize<'de> for RgbColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let [_, red, green, blue] = value
            .strip_prefix('#')
            .filter(|hex| {
                hex.len() == 6
                    && hex
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'F'))
            })
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .ok_or_else(|| de::Error::custom("colour must be #RRGGBB in uppercase hexadecimal"))?
            .to_be_bytes();
        Ok(Self { red, green, blue })
    }
}

impl ContentSnapshotData {
    /// An Accessibility snapshot, written as v4 without `lines`.
    #[must_use]
    pub fn new(
        text: Option<String>,
        chars: u64,
        cutoff: Option<ContentSnapshotCutoff>,
        trigger: ContentSnapshotTrigger,
    ) -> Self {
        Self {
            text,
            chars,
            trigger,
            shape: ContentSnapshotShape::Current { cutoff },
        }
    }

    /// A text-recognition snapshot, written as v4 with `lines` in the order of `text`.
    #[must_use]
    pub fn recognized(
        text: Option<String>,
        chars: u64,
        cutoff: Option<ContentSnapshotCutoff>,
        trigger: ContentSnapshotTrigger,
        lines: Vec<ContentSnapshotLine>,
    ) -> Self {
        Self {
            text,
            chars,
            trigger,
            shape: ContentSnapshotShape::Recognized {
                cutoff,
                lines: Some(lines),
            },
        }
    }

    /// Distinguishes a retained v2 payload (`None`) from both a completed current
    /// payload (`Some(None)`) and a cut-off current payload (`Some(Some(reason))`).
    #[must_use]
    pub const fn cutoff(&self) -> Option<Option<ContentSnapshotCutoff>> {
        match self.shape {
            ContentSnapshotShape::Legacy { .. } => None,
            ContentSnapshotShape::Current { cutoff }
            | ContentSnapshotShape::Recognized { cutoff, .. } => Some(cutoff),
        }
    }

    /// Returns the completion marker retained from a v2 payload. Current
    /// payloads return `None` because `cutoff` is their single source of truth.
    #[must_use]
    pub const fn legacy_complete(&self) -> Option<bool> {
        match self.shape {
            ContentSnapshotShape::Legacy { complete } => Some(complete),
            ContentSnapshotShape::Current { .. } | ContentSnapshotShape::Recognized { .. } => None,
        }
    }

    pub(super) const fn is_legacy(&self) -> bool {
        matches!(self.shape, ContentSnapshotShape::Legacy { .. })
    }

    /// `None` when the payload has no `lines` field; `Some(None)` when the core
    /// size safety rule removed them.
    pub(crate) const fn lines(&self) -> Option<&Option<Vec<ContentSnapshotLine>>> {
        match &self.shape {
            ContentSnapshotShape::Recognized { lines, .. } => Some(lines),
            ContentSnapshotShape::Legacy { .. } | ContentSnapshotShape::Current { .. } => None,
        }
    }

    pub(crate) fn lines_mut(&mut self) -> Option<&mut Option<Vec<ContentSnapshotLine>>> {
        match &mut self.shape {
            ContentSnapshotShape::Recognized { lines, .. } => Some(lines),
            ContentSnapshotShape::Legacy { .. } | ContentSnapshotShape::Current { .. } => None,
        }
    }
}

impl<'de> Deserialize<'de> for ContentSnapshotData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match ContentSnapshotWireData::deserialize(deserializer)? {
            ContentSnapshotWireData::Current(data) => Self {
                text: data.text,
                chars: data.chars,
                trigger: data.trigger,
                shape: match data.lines {
                    None => ContentSnapshotShape::Current {
                        cutoff: data.cutoff,
                    },
                    Some(lines) => ContentSnapshotShape::Recognized {
                        cutoff: data.cutoff,
                        lines,
                    },
                },
            },
            ContentSnapshotWireData::Legacy(data) => Self {
                text: data.text,
                chars: data.chars,
                trigger: data.trigger,
                shape: ContentSnapshotShape::Legacy {
                    complete: data.complete,
                },
            },
        })
    }
}

impl Serialize for ContentSnapshotData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (cutoff, lines) = match &self.shape {
            ContentSnapshotShape::Legacy { complete } => {
                return LegacyContentSnapshotDataRef {
                    text: &self.text,
                    chars: self.chars,
                    complete: *complete,
                    trigger: self.trigger,
                }
                .serialize(serializer);
            }
            ContentSnapshotShape::Current { cutoff } => (*cutoff, None),
            ContentSnapshotShape::Recognized { cutoff, lines } => (*cutoff, Some(lines)),
        };
        CurrentContentSnapshotDataRef {
            text: &self.text,
            chars: self.chars,
            cutoff,
            trigger: self.trigger,
            lines,
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ContentSnapshotWireData {
    Current(CurrentContentSnapshotData),
    Legacy(LegacyContentSnapshotData),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentContentSnapshotData {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    text: Option<String>,
    chars: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    cutoff: Option<ContentSnapshotCutoff>,
    trigger: ContentSnapshotTrigger,
    /// An absent field (`None`) differs from a present null (`Some(None)`).
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    lines: Option<Option<Vec<ContentSnapshotLine>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyContentSnapshotData {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    text: Option<String>,
    chars: u64,
    complete: bool,
    trigger: ContentSnapshotTrigger,
}

#[derive(Serialize)]
struct CurrentContentSnapshotDataRef<'a> {
    text: &'a Option<String>,
    chars: u64,
    cutoff: Option<ContentSnapshotCutoff>,
    trigger: ContentSnapshotTrigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    lines: Option<&'a Option<Vec<ContentSnapshotLine>>>,
}

#[derive(Serialize)]
struct LegacyContentSnapshotDataRef<'a> {
    text: &'a Option<String>,
    chars: u64,
    complete: bool,
    trigger: ContentSnapshotTrigger,
}

fn deserialize_present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_window_fraction_survives_a_json_round_trip_on_its_grid() {
        // Catches a scale or rounding error that changes a fraction between the writer
        // and a later read of the stored row.
        for ten_thousandths in 0..=10_000_u16 {
            let fraction = WindowFraction(ten_thousandths);
            let json = serde_json::to_string(&fraction).expect("serialize fraction");
            assert_eq!(
                serde_json::from_str::<WindowFraction>(&json).expect("read fraction back"),
                fraction,
                "{json}"
            );
        }
        assert_eq!(
            serde_json::from_str::<WindowFraction>("0.12346")
                .expect("off-grid fraction")
                .ratio(),
            0.1235
        );
        assert!(serde_json::from_str::<WindowFraction>("1.0001").is_err());
        assert!(WindowFraction::from_ratio(-0.0001).is_none());
        assert!(WindowFraction::from_ratio(f64::NAN).is_none());
    }

    #[test]
    fn colours_use_uppercase_hexadecimal_only() {
        // Catches lowercase or malformed colours slipping past the published pattern.
        let colour = RgbColor {
            red: 0x66,
            green: 0xDB,
            blue: 0x0E,
        };
        assert_eq!(serde_json::to_value(colour).expect("colour"), "#66DB0E");
        assert_eq!(
            serde_json::from_value::<RgbColor>("#66DB0E".into()).expect("colour"),
            colour
        );
        for invalid in [
            "#66db0e", "66DB0E", "#66DB0", "#66DB0E0", "#GGGGGG", "#+6DB0E", "#６DB0E",
        ] {
            assert!(
                serde_json::from_value::<RgbColor>(invalid.into()).is_err(),
                "{invalid}"
            );
        }
    }
}
