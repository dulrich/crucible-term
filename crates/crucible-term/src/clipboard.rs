//! Clipboard policy and the paste transform: pure functions, tested without a compositor.

use crucible_term_core::Event;

/// The MIME types offered for (and accepted from) the clipboard and primary selection, in
/// preference order.
pub const MIME_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// The first of our MIME types an offer advertises.
pub fn pick_mime(offered: &[String]) -> Option<&'static str> {
    MIME_TYPES
        .into_iter()
        .find(|m| offered.iter().any(|o| o == m))
}

/// What to do with an OSC 52 event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Osc52 {
    /// Put `text` on the clipboard.
    Clipboard(String),
    /// Put `text` on the primary selection.
    Primary(String),
    /// Do nothing (and say nothing).
    Ignore,
}

/// OSC 52 policy (foot's rule): a store is honoured only while the window has keyboard focus,
/// so a background program cannot overwrite the clipboard. A read (`ClipboardLoad`) is off: it
/// is ignored and never answered. Other events are not clipboard business.
pub fn osc52_policy(event: &Event, focused: bool) -> Osc52 {
    match event {
        Event::ClipboardStore { primary, text } if focused => {
            if *primary {
                Osc52::Primary(text.clone())
            } else {
                Osc52::Clipboard(text.clone())
            }
        }
        _ => Osc52::Ignore,
    }
}

/// The bytes to write to the child for pasted `data`. Invalid UTF-8 is lossy-converted first.
/// With bracketed paste on the payload is wrapped in `ESC[200~ ... ESC[201~` after every ESC is
/// stripped (so the payload cannot contain `ESC[201~` or any other escape sequence); with it
/// off, line breaks (`\r\n` and `\n`) become `\r`.
pub fn paste_bytes(data: &[u8], bracketed: bool) -> Vec<u8> {
    let text = String::from_utf8_lossy(data);
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend(text.replace('\x1b', "").into_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_transform() {
        assert_eq!(paste_bytes(b"a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(paste_bytes(b"a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        // An embedded end marker (and any bare ESC) cannot escape the bracket.
        assert_eq!(
            paste_bytes(b"x\x1b[201~rm -rf\x1b", true),
            b"\x1b[200~x[201~rm -rf\x1b[201~"
        );
        // Invalid UTF-8 is replaced, not rejected.
        assert_eq!(paste_bytes(b"a\xffb", false), "a\u{fffd}b".as_bytes());
    }

    #[test]
    fn osc52_requires_focus_and_never_reads() {
        let store = |primary| Event::ClipboardStore {
            primary,
            text: "t".into(),
        };
        assert_eq!(
            osc52_policy(&store(false), true),
            Osc52::Clipboard("t".into())
        );
        assert_eq!(osc52_policy(&store(true), true), Osc52::Primary("t".into()));
        assert_eq!(osc52_policy(&store(false), false), Osc52::Ignore);
        let load = Event::ClipboardLoad { primary: false };
        assert_eq!(osc52_policy(&load, true), Osc52::Ignore);
        assert_eq!(osc52_policy(&Event::Bell, true), Osc52::Ignore);
        let offered = vec!["text/html".to_string(), "text/plain".to_string()];
        assert_eq!(pick_mime(&offered), Some("text/plain"));
        assert_eq!(pick_mime(&[]), None);
    }
}
