//! Run a subprocess with a hard timeout. Secrets are only ever passed on stdin, never argv or env.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

pub struct Output {
    /// Exit code; `-1` when the process was ended by a signal.
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn run(program: &Path, args: &[&str], stdin: Option<&[u8]>, timeout: Duration) -> Result<Output> {
    let what = program.display().to_string();
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::io(format!("start {what}"), e))?;

    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });

    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A broken pipe means the child exited early; its exit code tells the story.
        let _ = pipe.write_all(data);
    }

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|e| Error::io(format!("wait for {what}"), e))? {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(Error::Timeout(timeout));
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    Ok(Output {
        code: status.code().unwrap_or(-1),
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let o = run(Path::new("/bin/sh"), &["-c", "echo hi; echo oops >&2; exit 3"], None, Duration::from_secs(5)).unwrap();
        assert_eq!(o.code, 3);
        assert_eq!(o.stdout, b"hi\n");
        assert_eq!(o.stderr, b"oops\n");
    }

    #[test]
    fn passes_stdin() {
        let o = run(Path::new("/bin/cat"), &[], Some(b"secret"), Duration::from_secs(5)).unwrap();
        assert_eq!(o.stdout, b"secret");
    }

    #[test]
    fn kills_a_hung_process() {
        let t = Instant::now();
        let err = run(Path::new("/bin/sleep"), &["30"], None, Duration::from_millis(200)).err().expect("timeout");
        assert!(matches!(err, Error::Timeout(_)));
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
