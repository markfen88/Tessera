//! Making daemon-supplied text safe to print on a terminal.
//!
//! Device names, channel labels and remote names come from the network or
//! from other users; they can contain terminal escape sequences, newlines
//! that fake extra output lines, or bidirectional-override characters that
//! reorder what the operator reads. Everything the CLI prints in human form
//! goes through [`cleaned`] first: unsafe characters become visible
//! `\u{..}` escapes. (JSON output is left alone: JSON string escaping already
//! covers control characters and consumers parse it.)

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Must this character be escaped before printing?
#[must_use]
pub fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            u32::from(c),
            0x061C
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0xFEFF
        )
}

/// Escape unsafe characters (and the backslash) as `\u{hex}` / `\\`.
#[must_use]
pub fn clean(s: &str) -> String {
    clean_with(s, false)
}

/// Like [`clean`] but keeps newlines and tabs (multi-line text such as a
/// configuration file).
#[must_use]
pub fn clean_text(s: &str) -> String {
    clean_with(s, true)
}

fn clean_with(s: &str, keep_layout: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if keep_layout && (c == '\n' || c == '\t') {
            out.push(c);
        } else if c == '\\' && !keep_layout {
            out.push_str("\\\\");
        } else if is_unsafe(c) {
            out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

fn clean_value(v: &mut Value) {
    match v {
        Value::String(s) => *s = clean(s),
        Value::Array(a) => a.iter_mut().for_each(clean_value),
        Value::Object(o) => o.iter_mut().for_each(|(_, v)| clean_value(v)),
        _ => {}
    }
}

/// A copy of `t` with every string field cleaned. Works for any serde type
/// (including variants added later); falls back to the original if the round
/// trip fails.
#[must_use]
pub fn cleaned<T: Serialize + DeserializeOwned + Clone>(t: &T) -> T {
    let Ok(mut v) = serde_json::to_value(t) else {
        return t.clone();
    };
    clean_value(&mut v);
    serde_json::from_value(v).unwrap_or_else(|_| t.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tessera_control_types::{DeviceKey, Event, Origin};

    #[test]
    fn escape_sequences_and_line_breaks_are_neutralised() {
        let s = "x\u{1b}[2J\nFAKE LINE\r";
        let c = clean(s);
        assert!(!c.contains('\u{1b}') && !c.contains('\n') && !c.contains('\r'));
        assert!(c.contains("\\u{1b}[2J\\u{a}FAKE LINE\\u{d}"));
    }

    #[test]
    fn bidi_overrides_are_visible() {
        for c in [
            '\u{202e}', '\u{2067}', '\u{200f}', '\u{061c}', '\u{feff}', '\u{85}',
        ] {
            assert!(clean(&format!("a{c}b")).contains("\\u{"), "{c:?}");
        }
        assert_eq!(clean("ünï 音"), "ünï 音");
    }

    #[test]
    fn text_keeps_layout_but_not_escapes() {
        assert_eq!(clean_text("a\n\tb\u{1b}c"), "a\n\tb\\u{1b}c");
    }

    #[test]
    fn cleaning_a_whole_event_reaches_every_string() {
        let e = Event::DeviceRenamed {
            device: DeviceKey("d\u{202e}".into()),
            name: "n\n".into(),
            origin: Origin::Api {
                principal: "p\u{1b}".into(),
            },
        };
        let c = cleaned(&e);
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains('\u{202e}') && !json.contains("\\n") && !json.contains("\\u001b"));
        assert!(json.contains("\\\\u{202e}"), "{json}");
    }

    proptest! {
        #[test]
        fn nothing_unsafe_survives(s in ".*") {
            prop_assert!(!clean(&s).chars().any(is_unsafe));
            prop_assert!(!clean_text(&s).chars().any(|c| is_unsafe(c) && c != '\n' && c != '\t'));
        }
    }
}
