//! Where Bench keeps things on disk, on every platform it builds for.
//!
//! Three places, and the reasoning for each:
//!
//! - **Config** (`credentials.json`): the platform's per-user configuration
//!   directory — `$XDG_CONFIG_HOME` or `~/.config` on Unix, `%APPDATA%` on
//!   Windows. It holds a long-lived token, so it belongs where the OS keeps
//!   private per-user state, not in Documents.
//! - **Documents** (`Bench/captures`, `Bench/profiles`): the user's Documents
//!   folder, because these are the user's files — things to open in other
//!   programs, back up, or send to someone.
//! - **Home** underpins both, and is the one that goes wrong: Windows does not
//!   set `HOME`, it sets `USERPROFILE`. Reading `HOME` alone put every capture
//!   and profile in `%TEMP%` on Windows, where the OS deletes them.
//!
//! Falling back to the temp directory when nothing is set keeps a headless or
//! sandboxed run working; the callers show the path they chose, so a surprise
//! location is visible rather than silent.

use std::path::PathBuf;

/// The user's home directory: `HOME`, then `USERPROFILE`, then the temp
/// directory.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// The per-user configuration directory, before Bench's own folder is added.
///
/// `$XDG_CONFIG_HOME` wins everywhere when set, since a user who sets it means
/// it. Otherwise `%APPDATA%` on Windows and `~/.config` elsewhere — including
/// macOS, where a dotfile under home is what a command-line-adjacent tool is
/// expected to use.
pub fn config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg);
    }
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(appdata);
        }
    }
    home_dir().join(".config")
}

/// `~/Documents/Bench` — one folder, no spaces, for everything the instrument
/// saves that is the user's to keep.
pub fn bench_documents_dir() -> PathBuf {
    home_dir().join("Documents").join("Bench")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Environment variables are process-wide, so these tests hold one lock
    // and restore what they change; the rest of the suite runs in parallel.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn with_env(vars: &[(&'static str, Option<&str>)], f: impl FnOnce()) {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let restore = Restore(
            vars.iter()
                .map(|(k, _)| (*k, std::env::var_os(k)))
                .collect(),
        );
        for (key, value) in vars {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        drop(restore);
    }

    #[test]
    fn home_prefers_home_then_userprofile_then_temp() {
        with_env(
            &[("HOME", Some("/h")), ("USERPROFILE", Some("C:\\Users\\u"))],
            || assert_eq!(home_dir(), PathBuf::from("/h")),
        );
        with_env(
            &[("HOME", None), ("USERPROFILE", Some("C:\\Users\\u"))],
            || assert_eq!(home_dir(), PathBuf::from("C:\\Users\\u")),
        );
        // An empty variable is as good as unset — `HOME=` happens in CI.
        with_env(&[("HOME", Some("")), ("USERPROFILE", None)], || {
            assert_eq!(home_dir(), std::env::temp_dir())
        });
    }

    #[test]
    fn documents_hang_off_home() {
        with_env(&[("HOME", Some("/h")), ("USERPROFILE", None)], || {
            assert_eq!(
                bench_documents_dir(),
                PathBuf::from("/h").join("Documents").join("Bench")
            )
        });
    }

    #[test]
    fn config_honours_xdg_then_the_platform_default() {
        with_env(
            &[
                ("XDG_CONFIG_HOME", Some("/xdg")),
                ("HOME", Some("/h")),
                ("APPDATA", Some("C:\\AppData")),
            ],
            || assert_eq!(config_dir(), PathBuf::from("/xdg")),
        );
        with_env(
            &[
                ("XDG_CONFIG_HOME", None),
                ("HOME", Some("/h")),
                ("APPDATA", Some("C:\\AppData")),
            ],
            || {
                let expected = if cfg!(windows) {
                    PathBuf::from("C:\\AppData")
                } else {
                    PathBuf::from("/h").join(".config")
                };
                assert_eq!(config_dir(), expected);
            },
        );
    }
}
