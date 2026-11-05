mod cli;
mod config;
mod expressions;
mod io;
mod options;
mod output;
mod preview;
mod rename;
mod runtime;
mod scan;
mod transforms;

use anyhow::Result;
pub(crate) use output::display_path;
use std::io::{BufWriter, IsTerminal as _, LineWriter};

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(error) if output::is_broken_pipe(&error) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let stderr = std::io::stderr();
            let mut writer = LineWriter::new(stderr.lock());
            output::report_error(&mut writer, error, stderr.is_terminal());
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<i32> {
    let environment = config::Environment::capture();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = BufWriter::new(stdout.lock());
    let mut err = LineWriter::new(stderr.lock());
    run_with_output(
        std::env::args_os().collect(),
        &environment,
        &mut out,
        &mut err,
        stdout.is_terminal(),
    )
}

fn run_with_output(
    argv: Vec<std::ffi::OsString>,
    environment: &config::Environment,
    out: &mut impl std::io::Write,
    err: &mut impl std::io::Write,
    colored: bool,
) -> Result<i32> {
    let result = (|| match cli::parse(argv, environment, out, err, colored)? {
        cli::Action::Run(options) => {
            runtime::run(*options, out, err, colored)?;
            Ok(0)
        }
        cli::Action::Exit(code) => Ok(code),
    })();
    // Always flush both streams, preserving any known command failure.
    let stdout_flush = out.flush();
    let stderr_flush = err.flush();
    if matches!(result, Ok(code) if code != 0) {
        return result;
    }
    let result = output::finish(result, stdout_flush);
    output::finish(result, stderr_flush.map_err(output::diagnostic_error))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter {
        fail_write: bool,
        kind: std::io::ErrorKind,
    }

    impl std::io::Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                Err(self.kind.into())
            } else {
                Ok(bytes.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(self.kind.into())
        }
    }

    #[test]
    fn command_output_reports_write_and_flush_failures() {
        for fail_write in [true, false] {
            for kind in [
                std::io::ErrorKind::BrokenPipe,
                std::io::ErrorKind::StorageFull,
            ] {
                let mut out = FailingWriter { fail_write, kind };
                let error = run_with_output(
                    vec!["ren".into(), "--help".into()],
                    &config::Environment::default(),
                    &mut out,
                    &mut Vec::new(),
                    false,
                )
                .unwrap_err();
                assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), kind);
                assert_eq!(
                    output::is_broken_pipe(&error),
                    kind == std::io::ErrorKind::BrokenPipe
                );
            }
        }
    }

    #[test]
    fn broken_pipe_flush_preserves_operation_errors() {
        let mut out = FailingWriter {
            fail_write: false,
            kind: std::io::ErrorKind::BrokenPipe,
        };
        let error = run_with_output(
            vec!["ren".into(), "--lower".into(), "--upper".into()],
            &config::Environment::default(),
            &mut out,
            &mut Vec::new(),
            false,
        )
        .unwrap_err();
        assert!(!output::is_broken_pipe(&error));
        assert!(error.to_string().contains("--lower"));
    }
}
