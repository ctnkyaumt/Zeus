//! Opens a local file whose opening must be seen to work, such as the log.
//!
//! `open::that_detached` reports success once a launcher starts, so on Linux
//! a `.log` file with no usable handler opened nothing and said nothing:
//! `xdg-open` outside GNOME and KDE runs the default application itself, and
//! runs a terminal program (Neovim is Omarchy's text editor) without a
//! terminal. Here each way of opening is waited for, and a failure moves on to
//! showing the file in its folder, so the reader always ends up somewhere, or
//! learns that nothing worked.

use std::path::Path;

/// Opens `path` in its default application or, failing that, shows it in its
/// folder. Blocks for up to a few seconds per attempt, so call it off the
/// interface thread. The error names the file so the reader can go there.
pub fn open_or_reveal(path: &Path) -> Result<(), String> {
    first_success(&attempts(path)).map_err(|error| {
        format!(
            "Could not open {}: {error}. Open it from its folder instead.",
            path.display()
        )
    })
}

type Attempt<'a> = Box<dyn Fn() -> Result<(), String> + 'a>;

/// Runs the attempts in order and stops at the first that works; if none
/// does, the last error explains why.
fn first_success(attempts: &[Attempt<'_>]) -> Result<(), String> {
    let mut last = String::from("no way to open files was found");
    for attempt in attempts {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) => {
                log::warn!("an open attempt failed: {error}");
                last = error;
            }
        }
    }
    Err(last)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn attempts(path: &Path) -> Vec<Attempt<'_>> {
    use std::process::Command;
    let mut attempts: Vec<Attempt<'_>> = vec![
        // GLib launches the default application itself, in a terminal when
        // its desktop entry asks for one, and exits non-zero when it cannot.
        Box::new(move || linux::launch(Command::new("gio").arg("open").arg(path))),
        // The file manager, with the file selected.
        Box::new(move || linux::show_items(path)),
    ];
    if let Some(folder) = path.parent() {
        attempts.push(Box::new(move || {
            linux::launch(Command::new("xdg-open").arg(folder))
        }));
    }
    attempts
}

/// macOS's `open` and Windows' `start` report a file nothing can open.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn attempts(path: &Path) -> Vec<Attempt<'_>> {
    let mut attempts: Vec<Attempt<'_>> = vec![Box::new(move || {
        open::that(path).map_err(|error| error.to_string())
    })];
    if let Some(folder) = path.parent() {
        attempts.push(Box::new(move || {
            open::that(folder).map_err(|error| error.to_string())
        }));
    }
    attempts
}

/// Opens the window's web links the way attachments and folders are opened.
///
/// eframe hands `ctx.open_url` to the `webbrowser` crate, which on Linux and
/// the BSDs starts the browser as a child of ZapFast and never waits for it,
/// so each clicked link left a defunct process behind until the app quit
/// (#446). On Windows, `webbrowser` fails when `UserChoice` has a stale
/// ProgId or when a browser command line quotes only part of a flag such as
/// `--user-data-dir="C:\..."`.
///
/// This takes every `OpenUrl` out of the frame's output before eframe sees it:
/// Linux and the BSDs open it with `open::that_detached`, whose launcher is
/// double-forked and reaped; Windows resolves the registered HTTP/HTTPS
/// command via `AssocQueryStringW`, parses Windows command-line quoting, and
/// spawns the browser detached (falling back to `open::that_detached`). Every
/// link, from message bodies to egui's own `Hyperlink`, goes through it.
/// macOS opens links through the system without a child process, so it keeps
/// eframe's own handling.
#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
pub struct DetachedLinks {
    open: fn(&str) -> std::io::Result<()>,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Default for DetachedLinks {
    fn default() -> Self {
        Self {
            open: |url| open::that_detached(url),
        }
    }
}

#[cfg(target_os = "windows")]
impl Default for DetachedLinks {
    fn default() -> Self {
        Self {
            open: windows::open_url,
        }
    }
}

#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
impl egui::plugin::Plugin for DetachedLinks {
    fn debug_name(&self) -> &'static str {
        "zapfast-links"
    }

    fn output_hook(&mut self, _ctx: &egui::Context, output: &mut egui::FullOutput) {
        output.platform_output.commands.retain(|command| {
            let egui::OutputCommand::OpenUrl(link) = command else {
                return true;
            };
            if let Err(error) = (self.open)(&link.url) {
                // The address may come from a message, so it is not logged.
                log::warn!("could not open a link: {error}");
            }
            false
        });
    }
}

#[cfg(target_os = "windows")]
fn parse_windows_command(template: &str, url: &str) -> Option<(String, Vec<String>)> {
    parse_windows_command_with(template, url, |path| Path::new(path).is_file())
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_command_with(
    template: &str,
    url: &str,
    exe_exists: impl Fn(&str) -> bool,
) -> Option<(String, Vec<String>)> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (program, raw_args) = match find_unquoted_executable(trimmed, &exe_exists) {
        Some((exe, rest)) => (exe, tokenize_windows_args(rest)),
        None => {
            let mut tokens = tokenize_windows_args(trimmed).into_iter();
            let exe = tokens.next()?;
            (exe, tokens.collect())
        }
    };

    if program.is_empty() {
        return None;
    }

    let mut replaced_any = false;
    let mut args = Vec::with_capacity(raw_args.len() + 1);
    for arg in raw_args {
        let (substituted, replaced) = substitute_placeholders(&arg, url);
        replaced_any |= replaced;
        args.push(substituted);
    }
    if !replaced_any {
        args.push(url.to_owned());
    }

    Some((program, args))
}

#[cfg(any(target_os = "windows", test))]
fn find_unquoted_executable<'a>(
    trimmed: &'a str,
    exe_exists: &impl Fn(&str) -> bool,
) -> Option<(String, &'a str)> {
    if trimmed.starts_with('"') || !trimmed.contains(char::is_whitespace) {
        return None;
    }

    if exe_exists(trimmed) {
        return Some((trimmed.to_owned(), ""));
    }

    let mut prev_whitespace = false;
    let mut candidate_ends = Vec::new();
    for (idx, ch) in trimmed.char_indices() {
        let is_ws = ch.is_whitespace();
        if is_ws && idx > 0 && !prev_whitespace {
            candidate_ends.push(idx);
        }
        prev_whitespace = is_ws;
    }

    for idx in candidate_ends.into_iter().rev() {
        let candidate = &trimmed[..idx];
        if exe_exists(candidate) {
            return Some((candidate.to_owned(), &trimmed[idx..]));
        }
    }

    None
}

#[cfg(any(target_os = "windows", test))]
fn tokenize_windows_args(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut in_quotes = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let mut backslashes = 1usize;
            while chars.peek() == Some(&'\\') {
                chars.next();
                backslashes += 1;
            }
            if chars.peek() == Some(&'"') {
                chars.next();
                current.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes.is_multiple_of(2) {
                    in_quotes = !in_quotes;
                } else {
                    current.push('"');
                }
            } else {
                current.extend(std::iter::repeat_n('\\', backslashes));
            }
            in_token = true;
        } else if ch == '"' {
            in_quotes = !in_quotes;
            in_token = true;
        } else if ch.is_whitespace() && !in_quotes {
            if in_token {
                tokens.push(std::mem::take(&mut current));
                in_token = false;
            }
        } else {
            current.push(ch);
            in_token = true;
        }
    }

    if in_token {
        tokens.push(current);
    }

    tokens
}

#[cfg(any(target_os = "windows", test))]
fn substitute_placeholders(arg: &str, url: &str) -> (String, bool) {
    let mut out = String::with_capacity(arg.len());
    let mut replaced = false;
    let mut chars = arg.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' && matches!(chars.peek(), Some('0' | '1' | 'l' | 'L')) {
            chars.next();
            out.push_str(url);
            replaced = true;
        } else {
            out.push(ch);
        }
    }
    (out, replaced)
}

#[cfg(target_os = "windows")]
mod windows {
    use super::parse_windows_command;
    use std::process::{Command, Stdio};

    const ASSOCF_IS_PROTOCOL: u32 = 0x1000;
    const ASSOCSTR_COMMAND: u32 = 1;
    const S_OK: i32 = 0;
    const S_FALSE: i32 = 1;

    #[link(name = "shlwapi")]
    unsafe extern "system" {
        fn AssocQueryStringW(
            flags: u32,
            str_type: u32,
            psz_assoc: *const u16,
            psz_extra: *const u16,
            psz_out: *mut u16,
            pcch_out: *mut u32,
        ) -> i32;
    }

    pub(super) fn open_url(url: &str) -> std::io::Result<()> {
        let Some(scheme) = url_scheme(url) else {
            return open::that_detached(url);
        };

        let Some(template) = resolve_command_template(scheme) else {
            log::warn!("could not resolve default browser command for {scheme}; falling back");
            return open::that_detached(url);
        };

        let Some((program, args)) = parse_windows_command(&template, url) else {
            log::warn!("could not parse default browser command for {scheme}; falling back");
            return open::that_detached(url);
        };

        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_child) => Ok(()),
            Err(error) => {
                // Never log the URL itself, which may come from a chat message.
                log::warn!("could not launch default browser ({error}); falling back");
                open::that_detached(url)
            }
        }
    }

    fn url_scheme(url: &str) -> Option<&'static str> {
        let (scheme, _) = url.split_once(':')?;
        if scheme.eq_ignore_ascii_case("https") {
            Some("https")
        } else if scheme.eq_ignore_ascii_case("http") {
            Some("http")
        } else {
            None
        }
    }

    fn resolve_command_template(scheme: &str) -> Option<String> {
        let schemes: &[&str] = if scheme == "https" {
            &["https", "http"]
        } else {
            &["http"]
        };
        let attempts = [
            (0, Some("open")),
            (ASSOCF_IS_PROTOCOL, Some("open")),
            (ASSOCF_IS_PROTOCOL, None),
        ];
        for &candidate_scheme in schemes {
            for (flags, extra) in attempts {
                if let Some(template) = query_assoc_command(candidate_scheme, flags, extra) {
                    return Some(template);
                }
            }
        }
        None
    }

    fn query_assoc_command(scheme: &str, flags: u32, extra: Option<&str>) -> Option<String> {
        let assoc = to_wide_null(scheme);
        let extra = extra.map(to_wide_null);
        let extra_ptr = extra.as_ref().map_or(std::ptr::null(), Vec::as_ptr);

        let mut len: u32 = 0;
        // SAFETY: `assoc` and `extra_ptr` (when non-null) are NUL-terminated
        // UTF-16 strings. Passing a null output buffer queries the required
        // UTF-16 length.
        let hr = unsafe {
            AssocQueryStringW(
                flags,
                ASSOCSTR_COMMAND,
                assoc.as_ptr(),
                extra_ptr,
                std::ptr::null_mut(),
                &raw mut len,
            )
        };
        if (hr != S_OK && hr != S_FALSE) || len == 0 {
            return None;
        }

        let mut buf = vec![0u16; len as usize];
        // SAFETY: `buf` has capacity for `len` UTF-16 code units as returned
        // by the preceding size query.
        let hr = unsafe {
            AssocQueryStringW(
                flags,
                ASSOCSTR_COMMAND,
                assoc.as_ptr(),
                extra_ptr,
                buf.as_mut_ptr(),
                &raw mut len,
            )
        };
        if hr != S_OK {
            return None;
        }

        let written = (len as usize).min(buf.len());
        let trimmed = match buf[..written].iter().position(|&c| c == 0) {
            Some(nul) => &buf[..nul],
            None => &buf[..written],
        };
        let command = String::from_utf16_lossy(trimmed);
        (!command.trim().is_empty()).then_some(command)
    }

    fn to_wide_null(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long a launcher gets to fail. One still running by then is taken
    /// to have started the program: some launchers run it in the foreground.
    pub(super) const LAUNCH_GRACE: Duration = Duration::from_secs(3);

    /// Starts a launcher and waits up to [`LAUNCH_GRACE`] for its verdict.
    pub(super) fn launch(command: &mut Command) -> Result<(), String> {
        let program = command.get_program().to_string_lossy().into_owned();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        let deadline = Instant::now() + LAUNCH_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(format!("{program} failed ({status})")),
                Ok(None) if Instant::now() >= deadline => {
                    // Still running: reap it whenever it ends so it leaves no
                    // zombie behind.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(error) => return Err(format!("{program}: {error}")),
            }
        }
    }

    /// Asks the file manager to show the file selected in its folder.
    pub(super) fn show_items(path: &Path) -> Result<(), String> {
        let uri = file_uri(path);
        let connection =
            zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
        connection
            .call_method(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                Some("org.freedesktop.FileManager1"),
                "ShowItems",
                &(vec![uri.as_str()], ""),
            )
            .map(|_| ())
            .map_err(|error| format!("the file manager: {error}"))
    }

    /// A `file://` URI for an absolute path, percent-encoding every byte
    /// outside the characters a path segment may carry as they are.
    pub(super) fn file_uri(path: &Path) -> String {
        use std::fmt::Write;
        let mut uri = String::from("file://");
        for byte in path.as_os_str().as_encoded_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                    uri.push(char::from(*byte));
                }
                _ => {
                    let _ = write!(uri, "%{byte:02X}");
                }
            }
        }
        uri
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn the_first_attempt_that_works_ends_the_chain() {
        let tried = Cell::new(0);
        let attempts: Vec<Attempt<'_>> = vec![
            Box::new(|| {
                tried.set(tried.get() + 1);
                Err("no handler".into())
            }),
            Box::new(|| {
                tried.set(tried.get() + 1);
                Ok(())
            }),
            Box::new(|| {
                tried.set(tried.get() + 1);
                Ok(())
            }),
        ];
        assert_eq!(first_success(&attempts), Ok(()));
        assert_eq!(tried.get(), 2, "the fallback ran and nothing after it");
    }

    #[test]
    fn when_nothing_works_the_last_reason_is_reported() {
        let attempts: Vec<Attempt<'_>> = vec![
            Box::new(|| Err("gio failed".into())),
            Box::new(|| Err("no file manager".into())),
        ];
        assert_eq!(first_success(&attempts), Err("no file manager".into()));
        assert!(first_success(&[]).is_err());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn file_uris_escape_what_a_path_segment_cannot_carry() {
        use linux::file_uri;
        assert_eq!(
            file_uri(Path::new("/home/ada/.local/state/zapfast/zapfast.log")),
            "file:///home/ada/.local/state/zapfast/zapfast.log"
        );
        assert_eq!(
            file_uri(Path::new("/home/José Ω/a#b%c.log")),
            "file:///home/Jos%C3%A9%20%CE%A9/a%23b%25c.log"
        );
    }

    #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
    #[test]
    fn links_are_opened_detached_instead_of_by_eframe() {
        use std::sync::Mutex;
        static OPENED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let ctx = egui::Context::default();
        ctx.add_plugin(DetachedLinks {
            open: |url| {
                OPENED.lock().unwrap().push(url.to_owned());
                Ok(())
            },
        });
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.ctx()
                .open_url(egui::OpenUrl::new_tab("https://example.invalid/a"));
            ui.ctx().copy_text("kept".into());
            ui.ctx()
                .open_url(egui::OpenUrl::same_tab("https://example.invalid/b"));
        });
        output.textures_delta.clear();
        assert_eq!(
            *OPENED.lock().unwrap(),
            ["https://example.invalid/a", "https://example.invalid/b"]
        );
        // eframe sees no link to open itself; other commands pass through.
        assert!(matches!(
            output.platform_output.commands.as_slice(),
            [egui::OutputCommand::CopyText(text)] if text == "kept"
        ));
    }

    /// A launcher's exit status decides: a failure is reported, a success
    /// or one still running after the grace period counts as opened.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_launcher_is_judged_by_its_exit() {
        use linux::{LAUNCH_GRACE, launch};
        use std::process::Command;
        use std::time::{Duration, Instant};
        assert_eq!(launch(Command::new("sh").args(["-c", "exit 0"])), Ok(()));
        let failed = launch(Command::new("sh").args(["-c", "exit 4"])).unwrap_err();
        assert!(failed.starts_with("sh failed"), "{failed}");
        let missing = launch(&mut Command::new("zapfast-no-such-launcher")).unwrap_err();
        assert!(
            missing.starts_with("zapfast-no-such-launcher: "),
            "{missing}"
        );
        let started = Instant::now();
        assert_eq!(launch(Command::new("sh").args(["-c", "sleep 30"])), Ok(()));
        assert!(started.elapsed() < LAUNCH_GRACE + Duration::from_secs(2));
    }

    #[test]
    fn windows_command_parses_launchbro_profile_flag() {
        let url = "https://example.invalid/path?a=1&b=2";
        let template = concat!(
            r#""C:\Users\user\Documents\launchbro\64\bin\chrome.exe" "#,
            r#"--user-data-dir="C:\Users\user\Documents\launchbro\64\profile" -- "%1""#,
        );
        let parsed = parse_windows_command_with(template, url, |_| false).unwrap();
        assert_eq!(
            parsed,
            (
                r"C:\Users\user\Documents\launchbro\64\bin\chrome.exe".into(),
                vec![
                    r"--user-data-dir=C:\Users\user\Documents\launchbro\64\profile".into(),
                    "--".into(),
                    url.into(),
                ],
            )
        );
    }

    #[test]
    fn windows_command_preserves_spaces_inside_quoted_flag_values() {
        let url = "https://example.invalid/";
        let template = concat!(
            r#""C:\Program Files\Browser\chrome.exe" "#,
            r#"--user-data-dir="C:\My Profiles\Work Profile" "%1""#,
        );
        let parsed = parse_windows_command_with(template, url, |_| false).unwrap();
        assert_eq!(
            parsed,
            (
                r"C:\Program Files\Browser\chrome.exe".into(),
                vec![
                    r"--user-data-dir=C:\My Profiles\Work Profile".into(),
                    url.into(),
                ],
            )
        );
    }

    #[test]
    fn windows_command_substitutes_all_placeholder_variants() {
        let url = "https://example.invalid/?q=%l";
        let template = r#""C:\App\browser.exe" --url="%1" --zero=%0 --lower=%l --upper=%L"#;
        let parsed = parse_windows_command_with(template, url, |_| false).unwrap();
        assert_eq!(
            parsed,
            (
                r"C:\App\browser.exe".into(),
                vec![
                    format!("--url={url}"),
                    format!("--zero={url}"),
                    format!("--lower={url}"),
                    format!("--upper={url}"),
                ],
            )
        );
    }

    #[test]
    fn windows_command_appends_url_when_placeholder_is_missing() {
        let url = "https://example.invalid/chat";
        let template = r#""C:\Program Files\Browser\chrome.exe" --profile-directory=Default"#;
        let parsed = parse_windows_command_with(template, url, |_| false).unwrap();
        assert_eq!(
            parsed,
            (
                r"C:\Program Files\Browser\chrome.exe".into(),
                vec!["--profile-directory=Default".into(), url.into()],
            )
        );
    }

    #[test]
    fn windows_command_parses_standard_firefox_and_edge_templates() {
        let url = "https://example.invalid/";
        let firefox = r#""C:\Program Files\Mozilla Firefox\firefox.exe" -osint -url "%1""#;
        assert_eq!(
            parse_windows_command_with(firefox, url, |_| false),
            Some((
                r"C:\Program Files\Mozilla Firefox\firefox.exe".into(),
                vec!["-osint".into(), "-url".into(), url.into()],
            ))
        );

        let edge = concat!(
            r#""C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe" "#,
            r"--single-argument %1",
        );
        assert_eq!(
            parse_windows_command_with(edge, url, |_| false),
            Some((
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe".into(),
                vec!["--single-argument".into(), url.into()],
            ))
        );
    }

    #[test]
    fn windows_command_finds_longest_unquoted_executable_with_spaces() {
        let url = "https://example.invalid/";
        let exists = |path: &str| {
            path == r"C:\Program" || path == r"C:\Program Files\My Browser\browser.exe"
        };

        let with_args = r#"C:\Program Files\My Browser\browser.exe --profile="Work Profile" "%1""#;
        assert_eq!(
            parse_windows_command_with(with_args, url, exists),
            Some((
                r"C:\Program Files\My Browser\browser.exe".into(),
                vec!["--profile=Work Profile".into(), url.into()],
            ))
        );

        let bare = r"C:\Program Files\My Browser\browser.exe";
        assert_eq!(
            parse_windows_command_with(bare, url, exists),
            Some((
                r"C:\Program Files\My Browser\browser.exe".into(),
                vec![url.into()],
            ))
        );
    }

    #[test]
    fn windows_command_handles_backslash_escaped_quotes() {
        let url = "https://example.invalid/";
        let template = r#""C:\App\browser.exe" --title="say \"hi\" \\" "%1""#;
        assert_eq!(
            parse_windows_command_with(template, url, |_| false),
            Some((
                r"C:\App\browser.exe".into(),
                vec![r#"--title=say "hi" \"#.into(), url.into()],
            ))
        );
    }

    #[test]
    fn windows_command_rejects_empty_or_whitespace_templates() {
        let url = "https://example.invalid/";
        assert_eq!(parse_windows_command_with("", url, |_| false), None);
        assert_eq!(
            parse_windows_command_with("   \t\n  ", url, |_| false),
            None
        );
        assert_eq!(parse_windows_command_with(r#""""#, url, |_| false), None);
    }
}
