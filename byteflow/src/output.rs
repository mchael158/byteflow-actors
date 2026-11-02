//! Host-side output for the `print` native.
//!
//! Bytecode must not own stdout. Embedders inject an [`OutputSink`]; the
//! default is [`NullSink`] so a library load is silent. CLI demos pass
//! [`StdoutSink`].

use crate::bytecode::Value;

/// Receives `print` native arguments. Must not panic.
///
/// The call may block for its own write. [`StdoutSink`] takes the process
/// stdout lock once, writes one line, and drops a failed write.
pub trait OutputSink: Send + Sync + std::fmt::Debug + 'static {
    fn write(&self, values: &[Value]);
}

/// Discard output (default for embedders and tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl OutputSink for NullSink {
    fn write(&self, _values: &[Value]) {}
}

/// Space-separated `Display` forms plus a newline on stdout.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    fn write(&self, values: &[Value]) {
        let line = format_print_line(values);
        let mut stdout = std::io::stdout().lock();
        let _ = std::io::Write::write_all(&mut stdout, line.as_bytes());
    }
}

/// One `print` record: values separated by a single space, then a newline.
fn format_print_line(values: &[Value]) -> String {
    use std::fmt::Write as _;

    let mut line = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        let _ = write!(line, "{value}");
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_line_is_one_record() {
        let list = match Value::try_list(vec![Value::Int(1)]) {
            Some(value) => value,
            None => panic!("list"),
        };
        let map = match Value::try_map(vec![(crate::bytecode::MapKey::Int(7), Value::Bool(true))]) {
            Some(value) => value,
            None => panic!("map"),
        };
        assert_eq!(format_print_line(&[]), "\n");
        assert_eq!(
            format_print_line(&[Value::Int(1), list, map]),
            "1 [1] {7: true}\n"
        );
    }

    #[test]
    fn stdout_sink_writes_one_line_without_panicking() {
        StdoutSink.write(&[Value::Int(1), Value::Bool(false)]);
    }
}
