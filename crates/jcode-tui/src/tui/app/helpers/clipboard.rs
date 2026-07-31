#[cfg(all(target_os = "linux", not(test)))]
use std::sync::{LazyLock, Mutex};
/// Keep Linux clipboard ownership alive for the TUI lifetime.
///
/// X11 and Wayland selections are hosted by their owner. A short-lived
/// `arboard::Clipboard` releases the copied text as soon as it drops unless a
/// clipboard manager happens to persist it.
#[cfg(all(target_os = "linux", not(test)))]
static LINUX_CLIPBOARD: LazyLock<Mutex<Option<arboard::Clipboard>>> =
    LazyLock::new(|| Mutex::new(None));
/// Test-only clipboard sink.
///
/// A headless CI runner has no Wayland socket, no X11 display, and a
/// non-terminal stdout, so every real clipboard path correctly fails and
/// `copy_to_clipboard` returns false. Tests that only care about shortcut
/// wiring (does Alt+S reach the copy handler with the right text?) then fail
/// for an environment reason rather than a code reason. Capturing into this
/// sink lets those tests assert the wiring *and* the copied text without
/// depending on a desktop session (refs #596).
#[cfg(test)]
static TEST_CLIPBOARD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Test-only: route clipboard writes into an in-process sink instead of the OS.
#[cfg(test)]
pub(crate) fn capture_clipboard_for_tests() {
    if let Ok(mut sink) = TEST_CLIPBOARD.lock() {
        *sink = Some(String::new());
    }
}

/// Test-only: the last text written while capture was enabled.
#[cfg(test)]
pub(crate) fn captured_clipboard_for_tests() -> Option<String> {
    let Ok(sink) = TEST_CLIPBOARD.lock() else {
        return None;
    };
    sink.clone()
}

/// Test-only: stop capturing and drop any captured text.
#[cfg(test)]
pub(crate) fn stop_capturing_clipboard_for_tests() {
    if let Ok(mut sink) = TEST_CLIPBOARD.lock() {
        *sink = None;
    }
}

/// Copy text to the system clipboard.
///
/// Linux uses the native Wayland/X11 clipboard when a display is present. OSC
/// 52 is reserved for headless or remote terminals, where no local clipboard
/// protocol can confirm ownership.
pub(in crate::tui::app) fn copy_to_clipboard(text: &str) -> bool {
    // Under test, never touch the OS clipboard. Beyond making results identical
    // on a desktop and a headless runner, the Linux path below spawns `wl-copy`,
    // which forks a clipboard server that does not exit; waiting on it hangs the
    // test binary indefinitely. Tests that assert copied text call
    // `capture_clipboard_for_tests` first and then read the sink; tests that
    // only assert "a copy happened" get a truthy result either way.
    #[cfg(test)]
    {
        if let Ok(mut sink) = TEST_CLIPBOARD.lock() {
            match sink.as_mut() {
                Some(captured) => {
                    captured.clear();
                    captured.push_str(text);
                }
                None => *sink = Some(text.to_string()),
            }
        }
        return true;
    }

    #[cfg(not(test))]
    {
        // On Windows, the native clipboard API must run before OSC 52. Writing an
        // OSC 52 sequence to stdout "succeeds" even when the console (conhost,
        // older Windows Terminal) silently ignores it, which reported "Copied"
        // while leaving the clipboard empty (issue #497). arboard talks to the
        // Win32 clipboard directly and is authoritative there.
        #[cfg(windows)]
        {
            if arboard::Clipboard::new()
                .and_then(|mut cb| cb.set_text(text.to_string()))
                .is_ok()
            {
                return true;
            }
            return copy_to_clipboard_osc52(text);
        }

        // Same class of bug on macOS: Apple Terminal (Terminal.app) silently
        // ignores OSC 52, yet writing the sequence to stdout "succeeds", so we
        // reported "Copied" while leaving the clipboard untouched. NSPasteboard
        // via arboard (with pbcopy as a belt-and-braces fallback) is authoritative
        // for local sessions; OSC 52 remains as the final remote-session fallback.
        #[cfg(target_os = "macos")]
        {
            if arboard::Clipboard::new()
                .and_then(|mut cb| cb.set_text(text.to_string()))
                .is_ok()
            {
                return true;
            }
            if let Ok(mut child) = std::process::Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                use std::io::Write;
                if let Some(stdin) = child.stdin.as_mut()
                    && stdin.write_all(text.as_bytes()).is_ok()
                {
                    drop(child.stdin.take());
                    if child.wait().map(|s| s.success()).unwrap_or(false) {
                        return true;
                    }
                }
            }
            return copy_to_clipboard_osc52(text);
        }

        #[cfg(target_os = "linux")]
        {
            return copy_to_linux_clipboard(text);
        }

        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        {
            copy_to_clipboard_osc52(text)
        }
    }
}

#[cfg(all(target_os = "linux", not(test)))]
fn copy_to_linux_clipboard(text: &str) -> bool {
    let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let has_x11 = std::env::var_os("DISPLAY").is_some();

    if has_wayland && copy_to_wayland_clipboard(text) {
        return true;
    }
    if has_wayland || has_x11 {
        return copy_to_retained_linux_clipboard(text);
    }
    copy_to_clipboard_osc52(text)
}

#[cfg(all(target_os = "linux", not(test)))]
fn copy_to_wayland_clipboard(text: &str) -> bool {
    let Ok(mut child) = std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    use std::io::Write;
    let Some(mut stdin) = child.stdin.take() else {
        return false;
    };
    if stdin.write_all(text.as_bytes()).is_err() {
        return false;
    }
    drop(stdin);

    // `wl-copy` keeps serving the selection after a successful write. Waiting
    // for that owner would stall the UI, so only wait long enough to catch an
    // immediate setup failure and reap a live owner in the background.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() >= deadline => {
                std::thread::spawn(move || {
                    if let Err(error) = child.wait() {
                        jcode_logging::debug(&format!("clipboard owner wait failed: {error}"));
                    }
                });
                return true;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => return false,
        }
    }
}

#[cfg(all(target_os = "linux", not(test)))]
fn copy_to_retained_linux_clipboard(text: &str) -> bool {
    let Ok(mut clipboard) = LINUX_CLIPBOARD.lock() else {
        return false;
    };
    if clipboard.is_none() {
        let Ok(new_clipboard) = arboard::Clipboard::new() else {
            return false;
        };
        *clipboard = Some(new_clipboard);
    }
    let copied = clipboard
        .as_mut()
        .is_some_and(|clipboard| clipboard.set_text(text.to_string()).is_ok());
    if !copied {
        // A disconnected backend must be recreated on the next copy attempt.
        *clipboard = None;
    }
    copied
}

#[cfg(not(test))]
/// Copy to clipboard using the OSC 52 terminal escape sequence. This asks the
/// terminal emulator to set the system clipboard without needing a local
/// display server, making it work over SSH, inside Docker, and under tmux
/// (with `set -g set-clipboard on`). Returns false if stdout is not a TTY.
fn copy_to_clipboard_osc52(text: &str) -> bool {
    use base64::Engine as _;
    use std::io::{IsTerminal, Write};

    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return false;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // OSC 52: ESC ] 52 ; c ; <base64> BEL
    let seq = format!("\x1b]52;c;{}\x07", encoded);
    out.write_all(seq.as_bytes()).is_ok() && out.flush().is_ok()
}
