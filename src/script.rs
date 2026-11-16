//! Running scripts with the program that runs them, rather than whatever
//! their file type opens: double-clicking a `.ps1` opens an editor, and a
//! `.sh` goes to whichever program claimed the extension last.
//!
//! Each kind goes to its interpreter - PowerShell, cmd, Git Bash or the
//! Python launcher - and runs from the script's own folder, the way Raycast
//! runs script commands. By default there is no window at all: the script
//! runs in the background, with its input closed, so one that stops to ask
//! for something ends instead of waiting, unseen, forever. A quicklink can
//! ask for a terminal instead (`window = 'keep'` or `'close'`).
//!
//! Scripts start through [`crate::launch`], so they run as the signed-in
//! user even though apex is elevated, unless run as administrator on
//! purpose.

use std::path::Path;

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::core::PCWSTR;

use crate::launch::{self, Show};

/// What a script's window does.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Window {
    /// There is none: the script runs in the background.
    #[default]
    Hidden,
    /// A terminal that stays open after the script ends, at a prompt in its
    /// folder, so its output can be read.
    Keep,
    /// A terminal that closes when the script ends.
    Close,
}

impl Window {
    /// A quicklink's `window` setting. Empty is the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "hidden" => Some(Self::Hidden),
            "keep" => Some(Self::Keep),
            "close" => Some(Self::Close),
            _ => None,
        }
    }

    /// As written to the config: nothing, for the default.
    pub fn setting(self) -> &'static str {
        match self {
            Self::Hidden => "",
            Self::Keep => "keep",
            Self::Close => "close",
        }
    }
}

/// Which interpreter a script needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    PowerShell,
    Batch,
    Bash,
    Python,
}

/// Script types by extension.
const KINDS: [(&str, Kind); 6] = [
    ("ps1", Kind::PowerShell),
    ("bat", Kind::Batch),
    ("cmd", Kind::Batch),
    ("sh", Kind::Bash),
    ("py", Kind::Python),
    ("pyw", Kind::Python),
];

fn kind(path: &str) -> Option<Kind> {
    let ext = Path::new(path).extension()?.to_str()?;
    KINDS
        .iter()
        .find(|(e, _)| ext.eq_ignore_ascii_case(e))
        .map(|(_, k)| *k)
}

/// Whether `path` names a script apex runs, by its extension.
pub fn is_script(path: &str) -> bool {
    kind(path).is_some()
}

/// Run the script at `path` with `args`, as `window` says: as the signed-in
/// user, or with `admin` as administrator. Returns whether it started.
///
/// A script with nothing on this machine to run it - no Git Bash, no
/// Python - is opened instead, the way double-clicking it would.
pub fn run(path: &str, args: &[String], window: Window, admin: bool) -> bool {
    let command = kind(path).and_then(|kind| {
        let interpreter = interpreter(kind, window)?;
        Some(command(kind, &interpreter, path, args, window))
    });
    match (command, admin) {
        (Some(c), false) => launch::start(&c.program, &c.args, &c.dir, c.show),
        (Some(c), true) => launch::start_as_admin(&c.program, &c.args, &c.dir, c.show),
        (None, false) => launch::open(path, None),
        (None, true) => launch::open_as_admin(path),
    }
}

/// A program to start, and how.
#[derive(Debug, PartialEq)]
struct Command {
    program: String,
    args: String,
    /// The script's own folder, where it runs.
    dir: String,
    show: Show,
}

/// The command line that runs the script at `path` with `interpreter`.
fn command(kind: Kind, interpreter: &str, path: &str, args: &[String], window: Window) -> Command {
    let mut rest = String::new();
    for arg in args {
        rest.push(' ');
        rest.push_str(&quote(arg));
    }
    let line = match kind {
        // -NonInteractive turns a prompt for input into an error. Bypass runs
        // the script whatever the execution policy, as a shortcut to it does:
        // choosing to run it is the decision the policy exists to ask for.
        Kind::PowerShell => {
            let mode = match window {
                Window::Hidden => " -NonInteractive",
                Window::Keep => " -NoExit",
                Window::Close => "",
            };
            format!(
                "-NoLogo -NoProfile{mode} -ExecutionPolicy Bypass -File {}{rest}",
                quote(path)
            )
        }
        // cmd parses its own command line, by rules of its own: see
        // `quote_cmd`. /s strips the outer pair of quotes, /d skips AutoRun
        // commands, and /k stays at a prompt afterwards.
        Kind::Batch => {
            let (switch, input) = match window {
                Window::Hidden => ('c', " <NUL"),
                Window::Keep => ('k', ""),
                Window::Close => ('c', ""),
            };
            let words: Vec<String> = std::iter::once(path)
                .chain(args.iter().map(String::as_str))
                .map(quote_cmd)
                .collect();
            format!(
                "/e:ON /v:OFF /d /s /{switch} \"{}{input}\"",
                words.join(" ")
            )
        }
        // The script's path with forward slashes, which bash takes as they
        // are. Keep runs a login shell afterwards, so the prompt is the
        // user's own.
        Kind::Bash => {
            let script = match window {
                Window::Hidden => r#"exec bash "$0" "$@" </dev/null"#,
                Window::Keep => r#"bash "$0" "$@"; exec bash --login -i"#,
                Window::Close => r#"exec bash "$0" "$@""#,
            };
            format!(
                "-c {} {}{rest}",
                quote(script),
                quote(&path.replace('\\', "/"))
            )
        }
        // -i leaves Python's own prompt open after the script.
        Kind::Python => {
            let keep = if window == Window::Keep { "-i " } else { "" };
            format!("{keep}{}{rest}", quote(path))
        }
    };
    Command {
        program: interpreter.to_string(),
        args: line,
        dir: Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .unwrap_or_default()
            .to_string(),
        show: if window == Window::Hidden {
            Show::Hidden
        } else {
            Show::Normal
        },
    }
}

/// One argument, quoted for a program that reads its command line the
/// Microsoft C runtime's way, as PowerShell, Python and Git Bash all do:
/// backslashes are literal except before a quote. Always quoted, which also
/// stops Git Bash expanding a `*` in it.
fn quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            // Each backslash before a quote doubles, and the quote itself is
            // escaped with one more.
            out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n('\\', backslashes));
        }
        backslashes = 0;
        out.push(c);
    }
    // Before the closing quote, too.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// One word of a batch file's command line, quoted for cmd: its quotes
/// doubled, and each `%` written as `%%cd:~,%`, which cmd reads back as a
/// lone `%` instead of expanding a variable from it - the escape Rust's own
/// `std::process` uses for batch files. With delayed expansion off a `!` is
/// literal, and `& | < > ^` are inert inside the quotes.
fn quote_cmd(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    for c in word.chars() {
        match c {
            '"' => out.push_str("\"\""),
            '%' => out.push_str("%%cd:~,%"),
            // A line break would end the command there; fields never hold
            // one, but a hand-written link might.
            '\r' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What runs a script of `kind` on this machine, if anything does.
fn interpreter(kind: Kind, window: Window) -> Option<String> {
    match kind {
        Kind::PowerShell => Some(powershell()),
        Kind::Batch => Some(system32("cmd.exe")),
        Kind::Bash => git_bash(),
        Kind::Python => python(window),
    }
}

/// PowerShell 7 when it is installed - the Store build through its execution
/// alias, whose path survives updates, or an MSI install - else Windows
/// PowerShell, which every Windows has.
fn powershell() -> String {
    on_path("pwsh.exe").unwrap_or_else(|| system32(r"WindowsPowerShell\v1.0\powershell.exe"))
}

/// Git for Windows' `bin\bash.exe`, which sets Git Bash's environment up -
/// never `System32\bash.exe`, which is WSL's and would run the script on
/// Linux, where its Windows paths mean nothing.
fn git_bash() -> Option<String> {
    let registered = [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER]
        .into_iter()
        .filter_map(|root| registry_string(root, r"SOFTWARE\GitForWindows", "InstallPath"));
    // git.exe on PATH lives in the install's `cmd` or `bin` folder.
    let beside_git = on_path("git.exe")
        .and_then(|git| Some(Path::new(&git).parent()?.parent()?.to_str()?.to_string()));
    let usual = [("ProgramFiles", "Git"), ("LOCALAPPDATA", r"Programs\Git")]
        .into_iter()
        .filter_map(|(var, sub)| Some(format!(r"{}\{sub}", std::env::var(var).ok()?)));
    registered
        .chain(beside_git)
        .chain(usual)
        .map(|root| format!(r"{}\bin\bash.exe", root.trim_end_matches(['\\', '/'])))
        .find(|bash| Path::new(bash).is_file())
}

/// The Python launcher, which picks a Python by the script's `#!` line, else
/// `python` itself. Without a window it is their windowless build, which has
/// no console to read from: a script that asks for input fails instead of
/// waiting.
fn python(window: Window) -> Option<String> {
    let (launcher, plain) = if window == Window::Hidden {
        ("pyw.exe", "pythonw.exe")
    } else {
        ("py.exe", "python.exe")
    };
    on_path(launcher).or_else(|| on_path(plain))
}

fn system32(file: &str) -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    format!(r"{root}\System32\{file}")
}

/// The first `exe` on PATH. Checked without following links: the Store's
/// app execution aliases are links that only process creation can open.
fn on_path(exe: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| !m.is_dir()))
        .and_then(|p| p.to_str().map(str::to_string))
}

/// A string value from the registry.
fn registry_string(root: HKEY, key: &str, name: &str) -> Option<String> {
    let key: Vec<u16> = key.encode_utf16().chain([0]).collect();
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 1024];
    let mut size = size_of_val(&buf) as u32;
    let status = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn scripts_are_known_by_their_extension() {
        assert_eq!(kind(r"C:\x\deploy.ps1"), Some(Kind::PowerShell));
        assert_eq!(kind(r"C:\x\BUILD.CMD"), Some(Kind::Batch));
        assert_eq!(kind(r"C:\x\run.bat"), Some(Kind::Batch));
        assert_eq!(kind("~/x/sync.sh"), Some(Kind::Bash));
        assert_eq!(kind(r"C:\x\tool.py"), Some(Kind::Python));
        assert_eq!(kind(r"C:\x\gui.pyw"), Some(Kind::Python));
        assert!(!is_script(r"C:\x\app.exe"));
        assert!(!is_script(r"C:\x.ps1\folder"));
        assert!(!is_script(r"C:\x\notes"));
    }

    #[test]
    fn the_window_setting_reads_three_ways_and_defaults_to_hidden() {
        assert_eq!(Window::parse(""), Some(Window::Hidden));
        assert_eq!(Window::parse(" Hidden "), Some(Window::Hidden));
        assert_eq!(Window::parse("KEEP"), Some(Window::Keep));
        assert_eq!(Window::parse("close"), Some(Window::Close));
        assert_eq!(Window::parse("kep"), None);
        // The default is never written to the config.
        assert_eq!(Window::Hidden.setting(), "");
        assert_eq!(Window::parse(Window::Keep.setting()), Some(Window::Keep));
    }

    #[test]
    fn quoting_follows_the_c_runtime() {
        assert_eq!(quote("plain"), r#""plain""#);
        assert_eq!(quote("a b"), r#""a b""#);
        assert_eq!(quote(""), r#""""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        // Backslashes are literal, except before a quote - including the
        // closing one.
        assert_eq!(quote(r"C:\dir\file"), r#""C:\dir\file""#);
        assert_eq!(quote(r"C:\trailing\"), r#""C:\trailing\\""#);
        assert_eq!(quote(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn quoting_for_cmd_defuses_percent_and_doubles_quotes() {
        assert_eq!(quote_cmd("a b & c"), r#""a b & c""#);
        assert_eq!(quote_cmd("100%"), r#""100%%cd:~,%""#);
        assert_eq!(quote_cmd("%PATH%"), r#""%%cd:~,%PATH%%cd:~,%""#);
        assert_eq!(quote_cmd(r#"say "hi""#), r#""say ""hi""""#);
        assert_eq!(quote_cmd("one\r\ntwo"), r#""one  two""#);
    }

    #[test]
    fn powershell_runs_the_file_hidden_without_prompting() {
        let args = strings(&["prod", "a b"]);
        let c = command(
            Kind::PowerShell,
            "pwsh.exe",
            r"C:\My Scripts\deploy.ps1",
            &args,
            Window::Hidden,
        );
        assert_eq!(c.program, "pwsh.exe");
        assert_eq!(
            c.args,
            r#"-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "C:\My Scripts\deploy.ps1" "prod" "a b""#
        );
        assert_eq!(c.dir, r"C:\My Scripts");
        assert_eq!(c.show, Show::Hidden);
        let keep = command(Kind::PowerShell, "pwsh.exe", r"C:\x.ps1", &[], Window::Keep);
        assert_eq!(
            keep.args,
            r#"-NoLogo -NoProfile -NoExit -ExecutionPolicy Bypass -File "C:\x.ps1""#
        );
        assert_eq!(keep.show, Show::Normal);
        let close = command(
            Kind::PowerShell,
            "pwsh.exe",
            r"C:\x.ps1",
            &[],
            Window::Close,
        );
        assert_eq!(
            close.args,
            r#"-NoLogo -NoProfile -ExecutionPolicy Bypass -File "C:\x.ps1""#
        );
    }

    #[test]
    fn batch_goes_through_cmd_with_its_input_closed_when_hidden() {
        let args = strings(&["100% & more", "x"]);
        let hidden = command(
            Kind::Batch,
            "cmd.exe",
            r"C:\a b\run.bat",
            &args,
            Window::Hidden,
        );
        assert_eq!(
            hidden.args,
            r#"/e:ON /v:OFF /d /s /c ""C:\a b\run.bat" "100%%cd:~,% & more" "x" <NUL""#
        );
        let keep = command(Kind::Batch, "cmd.exe", r"C:\run.cmd", &[], Window::Keep);
        assert_eq!(keep.args, r#"/e:ON /v:OFF /d /s /k ""C:\run.cmd"""#);
        let close = command(Kind::Batch, "cmd.exe", r"C:\run.cmd", &[], Window::Close);
        assert_eq!(close.args, r#"/e:ON /v:OFF /d /s /c ""C:\run.cmd"""#);
    }

    #[test]
    fn bash_gets_the_script_and_its_arguments_as_positionals() {
        let args = strings(&["a b"]);
        let hidden = command(
            Kind::Bash,
            "bash.exe",
            r"C:\dot files\sync.sh",
            &args,
            Window::Hidden,
        );
        assert_eq!(
            hidden.args,
            r#"-c "exec bash \"$0\" \"$@\" </dev/null" "C:/dot files/sync.sh" "a b""#
        );
        assert_eq!(hidden.dir, r"C:\dot files");
        let keep = command(Kind::Bash, "bash.exe", r"C:\s.sh", &[], Window::Keep);
        assert_eq!(
            keep.args,
            r#"-c "bash \"$0\" \"$@\"; exec bash --login -i" "C:/s.sh""#
        );
    }

    #[test]
    fn python_keeps_its_prompt_open_on_request() {
        let args = strings(&["x"]);
        let hidden = command(Kind::Python, "pyw.exe", r"C:\t.py", &args, Window::Hidden);
        assert_eq!(hidden.args, r#""C:\t.py" "x""#);
        let keep = command(Kind::Python, "py.exe", r"C:\t.py", &args, Window::Keep);
        assert_eq!(keep.args, r#"-i "C:\t.py" "x""#);
    }

    /// Each kind of script, really run hidden through the command lines
    /// above: every argument arrives intact, the script runs in its own
    /// folder, and asking for input ends it rather than hanging. Needs
    /// PowerShell, Git Bash and Python installed; run by hand:
    ///
    /// `cargo test -- --ignored scripts_really_run --nocapture`
    #[test]
    #[ignore]
    fn scripts_really_run() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("script probe");
        std::fs::create_dir_all(&dir).unwrap();
        let probes: [(&str, &str); 4] = [
            (
                "probe.ps1",
                "$out = foreach ($a in $args) { \"[$a]\" }\r\n\
                 $out += \"cwd=[$((Get-Location).Path)]\"\r\n\
                 try { Read-Host 'x' | Out-Null; $out += 'read=returned' } catch { $out += 'read=refused' }\r\n\
                 $out | Set-Content -Encoding UTF8 (Join-Path $PSScriptRoot 'out-ps1.txt')\r\n",
            ),
            (
                "probe.bat",
                "@echo off\r\n\
                 setlocal DisableDelayedExpansion\r\n\
                 set \"v1=%~1\"\r\nset \"v2=%~2\"\r\nset \"v3=%~3\"\r\nset \"v4=%~4\"\r\n\
                 setlocal EnableDelayedExpansion\r\n\
                 > \"%~dp0out-bat.txt\" (\r\n  echo([!v1!]\r\n  echo([!v2!]\r\n  echo([!v3!]\r\n  echo([!v4!]\r\n  echo(cwd=[!cd!]\r\n)\r\n\
                 pause > nul\r\n\
                 >> \"%~dp0out-bat.txt\" echo(read=refused\r\n",
            ),
            (
                "probe.sh",
                "for a in \"$@\"; do printf '[%s]\\n' \"$a\"; done > out-sh.txt\n\
                 printf 'cwd=[%s]\\n' \"$(pwd -W)\" >> out-sh.txt\n\
                 read -r x || echo read=refused >> out-sh.txt\n",
            ),
            (
                "probe.py",
                "import os, sys\n\
                 lines = ['[%s]' % a for a in sys.argv[1:]] + ['cwd=[%s]' % os.getcwd()]\n\
                 try:\n    input()\n    lines.append('read=returned')\n\
                 except Exception:\n    lines.append('read=refused')\n\
                 open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'out-py.txt'), 'w', encoding='utf-8').write('\\n'.join(lines) + '\\n')\n",
            ),
        ];
        let folder = dir.to_str().unwrap().to_string();
        // cmd hands a batch file its words verbatim, quotes included, so
        // the batch probe skips the one with quotes in it.
        let general = strings(&[
            "hello world",
            r#"say "hi" & 100%"#,
            r"C:\trailing\",
            "*.txt",
        ]);
        let batch = strings(&["hello world", "100% & %PATH%", r"C:\trailing\", "a^b|c<d>e"]);
        for (file, body) in probes {
            let path = dir.join(file);
            std::fs::write(&path, body).unwrap();
            let path = path.to_str().unwrap().to_string();
            let kind = kind(&path).unwrap();
            let out = dir.join(format!("out-{}.txt", &file[6..]));
            let _ = std::fs::remove_file(&out);
            let args = if kind == Kind::Batch {
                &batch
            } else {
                &general
            };
            let interpreter = interpreter(kind, Window::Hidden)
                .unwrap_or_else(|| panic!("nothing to run {file} with"));
            let c = command(kind, &interpreter, &path, args, Window::Hidden);
            eprintln!("{file}: {} {}", c.program, c.args);
            assert!(launch::start(&c.program, &c.args, &c.dir, c.show));

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let report = loop {
                if let Ok(text) = std::fs::read_to_string(&out)
                    && text.contains("read=")
                {
                    break text;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{file} never finished"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            };
            let report = report.trim_start_matches('\u{feff}').replace('/', r"\");
            let mut expected: Vec<String> = args
                .iter()
                .map(|a| format!("[{}]", a.replace('/', r"\")))
                .collect();
            expected.push(format!("cwd=[{folder}]"));
            expected.push("read=refused".to_string());
            let lines: Vec<&str> = report.lines().collect();
            assert_eq!(lines, expected, "{file}");
        }
    }
}
