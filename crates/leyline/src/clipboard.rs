const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasteRisk {
    Multiline,
    ControlCharacters,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PastePolicy {
    Allowed(String),
    NeedsConfirmation {
        text: String,
        bytes: usize,
        lines: usize,
        risk: PasteRisk,
    },
    Rejected,
}

#[must_use]
pub fn evaluate_paste(text: &str, confirm_multiline: bool) -> PastePolicy {
    if text.len() > MAX_CLIPBOARD_BYTES || text.contains('\0') {
        return PastePolicy::Rejected;
    }
    let normalized = text.replace("\r\n", "\n");
    let lines = normalized.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let controls = normalized
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'));
    if controls || (confirm_multiline && lines > 1) {
        let risk = if controls {
            PasteRisk::ControlCharacters
        } else {
            PasteRisk::Multiline
        };
        PastePolicy::NeedsConfirmation {
            bytes: normalized.len(),
            lines,
            risk,
            text: normalized,
        }
    } else {
        PastePolicy::Allowed(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiline_and_controls_do_not_bypass_confirmation() {
        assert!(matches!(
            evaluate_paste("one\ntwo", true),
            PastePolicy::NeedsConfirmation {
                risk: PasteRisk::Multiline,
                ..
            }
        ));
        assert!(matches!(
            evaluate_paste("echo\u{1b}[31m", false),
            PastePolicy::NeedsConfirmation {
                risk: PasteRisk::ControlCharacters,
                ..
            }
        ));
        assert_eq!(
            evaluate_paste("safe", true),
            PastePolicy::Allowed("safe".into())
        );
        assert_eq!(evaluate_paste("bad\0", false), PastePolicy::Rejected);
    }
}
