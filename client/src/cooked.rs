// Cooked mode emulation for --sync clients.
// When the worker requests cooked mode, the client buffers input locally,
// echoes characters, and sends complete lines on Enter.

pub struct CookedState {
    line_buf: Vec<u8>,
}

impl CookedState {
    pub fn new() -> Self {
        Self { line_buf: Vec::with_capacity(4096) }
    }

    /// Process one input byte. Returns bytes to send to worker stdin (if any).
    pub fn process(&mut self, byte: u8, local_stdout_fd: i32) -> Option<Vec<u8>> {
        match byte {
            0x7F => {
                // Backspace
                if !self.line_buf.is_empty() {
                    self.line_buf.pop();
                    super::write_fd(local_stdout_fd, b"\x08 \x08");
                }
                None
            }
            b'\r' | b'\n' => {
                super::write_fd(local_stdout_fd, b"\r\n");
                let mut to_send = self.line_buf.clone();
                to_send.push(b'\n');
                self.line_buf.clear();
                Some(to_send)
            }
            0x03 => {
                // Ctrl-C
                self.line_buf.clear();
                Some(b"\x03".to_vec())
            }
            0x04 => {
                // Ctrl-D on empty line → EOF
                if self.line_buf.is_empty() {
                    Some(vec![]) // signal to close stdin
                } else {
                    None
                }
            }
            b if b >= 0x20 => {
                if self.line_buf.len() < 4096 {
                    self.line_buf.push(b);
                    super::write_fd(local_stdout_fd, &[b]);
                }
                None
            }
            _ => None,
        }
    }
}
