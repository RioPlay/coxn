//! Poll the TUI for input while a blocking HTTP/SSE read waits for the next chunk.

use std::io;
#[cfg(test)]
use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

pub const IDLE_POLL_MS: u64 = 50;

/// Spawn a background thread that reads newline-delimited chunks from `reader`.
#[cfg(test)]
pub fn spawn_line_reader<R: Read + Send + 'static>(
    reader: R,
) -> mpsc::Receiver<io::Result<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Wait for the next line, calling `on_idle` on each timeout tick (~50ms).
/// Returns `Ok(None)` when the reader closed or `on_idle` returned false (cancel).
pub fn recv_line_with_idle(
    rx: &mpsc::Receiver<io::Result<String>>,
    on_idle: &mut Option<&mut dyn FnMut() -> bool>,
) -> Result<Option<String>, io::Error> {
    loop {
        match rx.recv_timeout(Duration::from_millis(IDLE_POLL_MS)) {
            Ok(Ok(line)) => return Ok(Some(line)),
            Ok(Err(e)) => return Err(e),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(idle) = on_idle.as_deref_mut()
                    && !idle()
                {
                    return Ok(None);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn recv_line_with_idle_reads_lines_until_disconnect() {
        let data = "one\ntwo\n";
        let rx = spawn_line_reader(Cursor::new(data));
        let mut idle_opt = None;
        let first = recv_line_with_idle(&rx, &mut idle_opt)
            .expect("read")
            .expect("line");
        assert_eq!(first, "one");
        let second = recv_line_with_idle(&rx, &mut idle_opt)
            .expect("read")
            .expect("line");
        assert_eq!(second, "two");
        let done = recv_line_with_idle(&rx, &mut idle_opt).expect("read");
        assert!(done.is_none());
    }

    #[test]
    fn recv_line_with_idle_stops_when_idle_returns_false() {
        let (tx, rx) = mpsc::channel::<io::Result<String>>();
        drop(tx);
        let mut stop = || false;
        let mut stop_opt = Some(&mut stop as &mut dyn FnMut() -> bool);
        let cancelled = recv_line_with_idle(&rx, &mut stop_opt).expect("read");
        assert!(cancelled.is_none());
    }
}
