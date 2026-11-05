// Low-level terminal primitives used by the interactive preview.
//
// Derived from fastmod (Copyright Meta Platforms, Inc. and affiliates),
// used under the Apache License, Version 2.0. See LICENSE and NOTICE
// at the repo root for details.

use std::io;

use crossterm::ExecutableCommand as _;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::ResetColor;
use crossterm::terminal::{Clear, ClearType};

pub(super) fn clear(out: &mut impl io::Write) -> io::Result<()> {
    out.execute(Clear(ClearType::All))?.execute(MoveTo(0, 0))?;
    Ok(())
}

/// Restore cursor visibility and colors even if rendering returns an error.
pub(super) struct HiddenCursor<'a, W: io::Write> {
    out: &'a mut W,
    visible: bool,
}

impl<'a, W: io::Write> HiddenCursor<'a, W> {
    pub(super) fn new(out: &'a mut W) -> io::Result<Self> {
        let guard = Self {
            out,
            visible: false,
        };
        guard.out.execute(Hide)?;
        Ok(guard)
    }

    pub(super) fn writer(&mut self) -> &mut W {
        self.out
    }

    pub(super) fn finish(mut self) -> io::Result<()> {
        self.out.execute(ResetColor)?.execute(Show)?;
        self.visible = true;
        Ok(())
    }
}

impl<W: io::Write> Drop for HiddenCursor<'_, W> {
    fn drop(&mut self) {
        if !self.visible {
            // Cleanup must attempt both commands and cannot replace the
            // original output error with a second error from the same writer.
            drop(self.out.execute(ResetColor));
            drop(self.out.execute(Show));
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use std::io::Write as _;

    #[derive(Default)]
    struct RecoveringWriter {
        output: Vec<u8>,
        fail_next: bool,
    }

    impl std::io::Write for RecoveringWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if std::mem::take(&mut self.fail_next) {
                return Err(std::io::Error::other("render failed"));
            }
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cursor_and_colors_are_restored_after_render_failure() {
        let mut out = RecoveringWriter::default();
        let render = |out: &mut RecoveringWriter| -> std::io::Result<()> {
            let mut cursor = super::HiddenCursor::new(out)?;
            cursor.writer().fail_next = true;
            cursor.writer().write_all(b"diff")?;
            cursor.finish()
        };
        assert_eq!(render(&mut out).unwrap_err().to_string(), "render failed");
        assert_eq!(out.output, b"\x1b[?25l\x1b[0m\x1b[?25h");
    }
}
