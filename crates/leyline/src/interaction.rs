use std::{ops::Range, sync::Arc};

const MAX_PREEDIT_BYTES: usize = 4096;
const MAX_PREEDIT_SCALARS: usize = 1024;
const MAX_IME_COMMIT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreeditOverlay {
    pub snapshot_generation: u64,
    pub revision: u64,
    pub anchor: [u16; 2],
    pub text: Arc<str>,
    pub cursor: PreeditCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreeditCursor {
    Hidden,
    Caret(u16),
    Selection(Range<u16>),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImeOutboundState {
    pub commit_serial: u32,
    pub dirty: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImeTransaction {
    pub preedit: Option<(String, Option<(i32, i32)>)>,
    pub commit: Option<String>,
    pub delete_surrounding: Option<(u32, u32)>,
}

#[derive(Clone, Debug, Default)]
pub struct ImeState {
    active: bool,
    pending: ImeTransaction,
    revision: u64,
    pub outbound: ImeOutboundState,
    pub preedit: Option<PreeditOverlay>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImeDone {
    pub commit: Option<Vec<u8>>,
    pub delete_ignored: bool,
    pub outbound_resend_required: bool,
}

#[allow(clippy::missing_errors_doc)]
impl ImeState {
    pub fn activate(&mut self) {
        self.active = true;
        self.outbound.dirty = true;
    }
    pub fn deactivate(&mut self) {
        self.active = false;
        self.pending = ImeTransaction::default();
        self.preedit = None;
    }
    pub fn sent_commit(&mut self) -> Result<u32, ImeError> {
        self.outbound.commit_serial = self
            .outbound
            .commit_serial
            .checked_add(1)
            .ok_or(ImeError::SerialOverflow)?;
        self.outbound.dirty = false;
        Ok(self.outbound.commit_serial)
    }
    pub fn preedit_string(
        &mut self,
        text: String,
        cursor: Option<(i32, i32)>,
    ) -> Result<(), ImeError> {
        self.ensure_active()?;
        if text.len() > MAX_PREEDIT_BYTES || text.chars().count() > MAX_PREEDIT_SCALARS {
            return Err(ImeError::PreeditTooLarge);
        }
        if let Some((begin, end)) = cursor {
            validate_cursor(&text, begin, end)?;
        }
        self.pending.preedit = Some((text, cursor));
        Ok(())
    }
    pub fn commit_string(&mut self, text: String) -> Result<(), ImeError> {
        self.ensure_active()?;
        if text.len() > MAX_IME_COMMIT_BYTES || text.contains('\0') {
            return Err(ImeError::CommitTooLarge);
        }
        self.pending.commit = Some(text);
        Ok(())
    }
    pub fn delete_surrounding_text(&mut self, before: u32, after: u32) -> Result<(), ImeError> {
        self.ensure_active()?;
        self.pending.delete_surrounding = Some((before, after));
        Ok(())
    }
    pub fn done(
        &mut self,
        serial: u32,
        generation: u64,
        anchor: [u16; 2],
    ) -> Result<ImeDone, ImeError> {
        self.ensure_active()?;
        let pending = std::mem::take(&mut self.pending);
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(ImeError::RevisionOverflow)?;
        self.preedit = pending.preedit.map(|(text, cursor)| PreeditOverlay {
            snapshot_generation: generation,
            revision: self.revision,
            anchor,
            cursor: cursor.map_or(PreeditCursor::Hidden, |(begin, end)| {
                if begin == end {
                    PreeditCursor::Caret(u16::try_from(begin).unwrap_or(u16::MAX))
                } else {
                    PreeditCursor::Selection(
                        u16::try_from(begin).unwrap_or(u16::MAX)
                            ..u16::try_from(end).unwrap_or(u16::MAX),
                    )
                }
            }),
            text: Arc::from(text),
        });
        let mismatch = serial != self.outbound.commit_serial;
        if mismatch {
            self.outbound.dirty = true;
        }
        Ok(ImeDone {
            commit: pending.commit.map(String::into_bytes),
            delete_ignored: pending.delete_surrounding.is_some(),
            outbound_resend_required: mismatch,
        })
    }
    fn ensure_active(&self) -> Result<(), ImeError> {
        if self.active {
            Ok(())
        } else {
            Err(ImeError::Inactive)
        }
    }
}

fn validate_cursor(text: &str, begin: i32, end: i32) -> Result<(), ImeError> {
    let begin = usize::try_from(begin).map_err(|_| ImeError::InvalidCursor)?;
    let end = usize::try_from(end).map_err(|_| ImeError::InvalidCursor)?;
    if begin > end
        || end > text.len()
        || !text.is_char_boundary(begin)
        || !text.is_char_boundary(end)
    {
        return Err(ImeError::InvalidCursor);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ImeError {
    #[error("text input is inactive")]
    Inactive,
    #[error("preedit exceeds its hard limit")]
    PreeditTooLarge,
    #[error("IME commit exceeds its hard limit or contains NUL")]
    CommitTooLarge,
    #[error("preedit cursor is not a valid UTF-8 byte range")]
    InvalidCursor,
    #[error("IME serial overflow")]
    SerialOverflow,
    #[error("IME revision overflow")]
    RevisionOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn done_applies_commit_even_when_serial_mismatches() {
        let mut ime = ImeState::default();
        ime.activate();
        assert_eq!(ime.sent_commit().unwrap(), 1);
        ime.commit_string("中文".into()).unwrap();
        let done = ime.done(99, 7, [2, 3]).unwrap();
        assert_eq!(done.commit.as_deref(), Some("中文".as_bytes()));
        assert!(done.outbound_resend_required && ime.outbound.dirty);
    }
    #[test]
    fn transaction_is_invisible_until_done_and_preedit_never_commits() {
        let mut ime = ImeState::default();
        ime.activate();
        ime.preedit_string("中".into(), Some((0, 3))).unwrap();
        assert!(ime.preedit.is_none());
        let done = ime.done(0, 4, [1, 1]).unwrap();
        assert!(done.commit.is_none());
        assert_eq!(ime.preedit.as_ref().unwrap().text.as_ref(), "中");
    }
    #[test]
    fn cursor_must_use_utf8_boundaries() {
        let mut ime = ImeState::default();
        ime.activate();
        assert_eq!(
            ime.preedit_string("中".into(), Some((1, 2))),
            Err(ImeError::InvalidCursor)
        );
    }
}
