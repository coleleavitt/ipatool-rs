//! Terminal prompts (Go `cmd/prompt.go`). Prompts go to stderr so structured output on
//! stdout stays valid; masked input echoes `*` per character.

use std::io::{self, BufRead, Read, Write};

/// Go's `readPrompt`. Errors carry Go's text.
pub fn read_prompt(prompt: &str, masked: bool) -> Result<String, String> {
    let raw_mode = if masked {
        Some(RawMode::enable()?)
    } else {
        None
    };
    let mut stderr = io::stderr();
    stderr
        .write_all(prompt.as_bytes())
        .and_then(|()| stderr.flush())
        .map_err(|err| format!("failed to write prompt: {err}"))?;
    if raw_mode.is_some() {
        let value = read_masked_input(io::stdin().lock(), &mut stderr);
        let _ = stderr.write_all(b"\r\n");
        return value;
    }
    read_line(&mut io::stdin().lock())
}

/// `bufio.Reader.ReadString('\n')` without the line ending; a last line without a
/// newline is an EOF error, as in Go.
pub fn read_line(input: &mut impl BufRead) -> Result<String, String> {
    let mut line = Vec::new();
    input
        .read_until(b'\n', &mut line)
        .map_err(|err| format!("failed to read input: {err}"))?;
    if line.pop() != Some(b'\n') {
        return Err("failed to read input: EOF".to_owned());
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Go's `bufio.Reader.ReadRune`: invalid UTF-8 reads as U+FFFD, one byte at a time.
fn read_rune(input: &mut impl Read) -> Result<char, String> {
    let mut bytes = [0u8; 4];
    let mut length = 0;
    loop {
        let mut byte = [0u8; 1];
        match input.read(&mut byte) {
            Ok(0) => return Err("failed to read input: EOF".to_owned()),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("failed to read input: {err}")),
        }
        bytes[length] = byte[0];
        length += 1;
        match std::str::from_utf8(&bytes[..length]) {
            Ok(text) => return Ok(text.chars().next().unwrap_or('\u{fffd}')),
            Err(err) if err.error_len().is_some() || length == 4 => return Ok('\u{fffd}'),
            Err(_) => {}
        }
    }
}

/// Go's `unicode.IsPrint`, close enough for typed passwords: no control characters and
/// no spaces other than U+0020.
fn is_print(key: char) -> bool {
    !key.is_control() && (key == ' ' || !key.is_whitespace())
}

/// Go's `readMaskedInput`: line editing (backspace, Ctrl-U), escape sequences ignored,
/// Ctrl-C cancels and Ctrl-D ends the input.
pub fn read_masked_input(mut input: impl Read, output: &mut impl Write) -> Result<String, String> {
    let mut password: Vec<char> = Vec::new();
    let mut escape = 0;
    loop {
        let key = read_rune(&mut input)?;
        // Skip terminal escape sequences such as arrow keys.
        if escape != 0 {
            if escape == 1 && (key == '[' || key == 'O') {
                escape = 2;
            } else if escape == 1 || ('\u{40}'..='\u{7e}').contains(&key) {
                escape = 0;
            }
            continue;
        }
        let echo = match key {
            '\r' | '\n' => return Ok(password.into_iter().collect()),
            '\u{3}' => return Err("input interrupted".to_owned()),
            '\u{4}' => return Err("EOF".to_owned()),
            '\u{1b}' => {
                escape = 1;
                String::new()
            }
            '\u{8}' | '\u{7f}' => {
                if password.pop().is_some() {
                    "\u{8} \u{8}".to_owned()
                } else {
                    String::new()
                }
            }
            '\u{15}' => {
                let echo = "\u{8} \u{8}".repeat(password.len());
                password.clear();
                echo
            }
            key if is_print(key) => {
                password.push(key);
                "*".to_owned()
            }
            _ => String::new(),
        };
        output
            .write_all(echo.as_bytes())
            .and_then(|()| output.flush())
            .map_err(|err| format!("failed to echo input: {err}"))?;
    }
}

/// `term.MakeRaw` on stdin, restored on drop.
#[cfg(unix)]
struct RawMode(rustix::termios::Termios);

#[cfg(unix)]
impl RawMode {
    fn enable() -> Result<Self, String> {
        use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};
        let stdin = io::stdin();
        let configure = |err: rustix::io::Errno| format!("failed to configure terminal: {err}");
        let original = tcgetattr(&stdin).map_err(configure)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(&stdin, OptionalActions::Now, &raw).map_err(configure)?;
        Ok(Self(original))
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ =
            rustix::termios::tcsetattr(io::stdin(), rustix::termios::OptionalActions::Now, &self.0);
    }
}

#[cfg(not(unix))]
struct RawMode;

#[cfg(not(unix))]
impl RawMode {
    fn enable() -> Result<Self, String> {
        Err("failed to configure terminal: masked input is not ported to this platform".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_masks_while_preserving_the_entered_password() {
        for (input, password, echo) in [
            ("secret\r", "secret", "******"),
            ("secret\n", "secret", "******"),
            ("pä密🔑\r", "pä密🔑", "****"),
            ("ab\x7fc\r", "ac", "**\x08 \x08*"),
            ("a密\x08c\r", "ac", "**\x08 \x08*"),
            ("\x08\x7fa\r", "a", "*"),
            ("ab\x15c\r", "c", "**\x08 \x08\x08 \x08*"),
            ("ab\x1b[D\x1bOCc\r", "abc", "***"),
            ("\r", "", ""),
        ] {
            let mut output = Vec::new();
            let value = read_masked_input(input.as_bytes(), &mut output).unwrap();
            assert_eq!(value, password, "{input:?}");
            assert_eq!(String::from_utf8(output).unwrap(), echo, "{input:?}");
        }
    }

    #[test]
    fn cancels_without_returning_the_password_on_ctrl_c() {
        assert_eq!(
            read_masked_input(&b"secret\x03"[..], &mut io::sink()),
            Err("input interrupted".to_owned())
        );
    }

    #[test]
    fn discards_incomplete_input() {
        for input in ["secret", "secret\x04"] {
            let error = read_masked_input(input.as_bytes(), &mut io::sink()).unwrap_err();
            assert!(error.contains("EOF"), "{input:?}: {error}");
        }
    }

    #[test]
    fn reads_lines_like_go() {
        assert_eq!(
            read_line(&mut &b"user@example.com\r\nnext"[..]).unwrap(),
            "user@example.com"
        );
        assert_eq!(
            read_line(&mut &b"+91 91234-56789\n"[..]).unwrap(),
            "+91 91234-56789"
        );
        assert_eq!(
            read_line(&mut &b""[..]).unwrap_err(),
            "failed to read input: EOF"
        );
        assert_eq!(
            read_line(&mut &b"partial"[..]).unwrap_err(),
            "failed to read input: EOF"
        );
    }
}
