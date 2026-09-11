//! Safe URL opening for the shell.
//!
//! Item URLs come from the network (RSS/Atom feeds, HN). Handing them to
//! `cmd.exe /c start "" <url>` lets `&` split it into extra commands and lets
//! quoting truncate the string — command injection from feed content. This
//! calls the OS shell handler directly instead: `ShellExecuteW` with the
//! `"open"` verb, no command interpreter involved.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Only `http://`/`https://` may be opened. Checked before any OS call, so a
/// scheme like `javascript:`, `file:` or `ftp:` never reaches `ShellExecuteW`.
pub fn validate_url(url: &str) -> Result<(), String> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Ok(())
    } else {
        Err(format!("unsupported URL scheme: {url}"))
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Opens `url` in the user's default browser via the shell's `"open"` verb.
pub fn open_url(url: &str) -> Result<(), String> {
    validate_url(url)?;
    // Validated and launched must be the same string: `validate_url` checks
    // the trimmed scheme, so the untrimmed original could carry leading
    // whitespace (or worse) straight into `ShellExecuteW`.
    let url = url.trim();
    let file = wide(url);
    let verb = wide("open");
    // SAFETY: all pointers are to null-terminated wide buffers kept alive for
    // the duration of the call; hwnd/params/dir are null as the API allows.
    let result = unsafe {
        ShellExecuteW(ptr::null_mut(), verb.as_ptr(), file.as_ptr(), ptr::null(), ptr::null(), SW_SHOWNORMAL as i32)
    };
    // Per ShellExecuteW docs: success returns a value > 32; anything else is an error code.
    if (result as isize) <= 32 {
        Err(format!("ShellExecuteW failed (code {})", result as isize))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_javascript_scheme() {
        assert!(validate_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn rejects_file_scheme() {
        assert!(validate_url("file:///c:/x").is_err());
    }

    #[test]
    fn rejects_ftp_scheme() {
        assert!(validate_url("ftp://x").is_err());
    }

    #[test]
    fn accepts_http_and_https() {
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn ampersand_in_query_passes_scheme_validation() {
        // Must pass validation without ever reaching ShellExecuteW/cmd.exe -
        // this is the string that broke `cmd /c start "" <url>`.
        assert!(validate_url("https://example.com/x?a=1&b=2&c=calc.exe").is_ok());
    }
}
