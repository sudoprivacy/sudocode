//! Cross-platform user-path resolution: the single place that answers "where is
//! the user's home directory" and "what does a leading `~` mean".
//!
//! `HOME` is a POSIX convention. Windows does not set it — it exposes
//! `USERPROFILE` instead — so any code that reads only `HOME` silently breaks
//! on Windows: a `~/...` path stops expanding (or expands to the drive root
//! when the empty string is substituted), and credential files resolved that
//! way are simply not found. That bug was found and fixed once already (see
//! the note in `api::providers::prompt_cache` about the omitted `USERPROFILE`
//! fallback) and then reappeared in three more call sites, because each one
//! open-coded its own lookup. Hence this module: one implementation, so a
//! platform fix lands everywhere at once.

use std::path::PathBuf;

/// The user's home directory, or `None` when the platform exposes neither
/// variable (an empty value counts as unset).
///
/// `HOME` wins when both are present: on Windows shells that emulate POSIX
/// (Git Bash, MSYS) `HOME` is the deliberate override, and honouring it keeps
/// a single session's paths consistent with the rest of its tooling.
#[inline]
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Expand a leading `~` to [`home_dir`], leaving the path untouched when there
/// is no home to expand against.
///
/// Only a *leading* `~` is a home reference: a bare `~`, or one followed by a
/// separator. `~` elsewhere in the path is an ordinary character (and is a
/// legal filename byte), so a blanket substitution would corrupt such paths.
/// Both separators are accepted on Windows, where `~\.codex` is as natural to
/// type as `~/.codex`.
#[inline]
#[must_use]
pub fn expand_tilde(path: &str) -> PathBuf {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| {
            if cfg!(windows) {
                path.strip_prefix("~\\")
            } else {
                None
            }
        })
    };
    match (rest, home_dir()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_tilde, home_dir};
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// These tests mutate process-wide environment variables, so they cannot
    /// overlap with each other.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Restores both variables on drop so a panicking assertion cannot leak
    /// state into the next test.
    struct HomeEnv {
        home: Option<std::ffi::OsString>,
        userprofile: Option<std::ffi::OsString>,
    }

    impl HomeEnv {
        fn set(home: Option<&str>, userprofile: Option<&str>) -> Self {
            let saved = Self {
                home: std::env::var_os("HOME"),
                userprofile: std::env::var_os("USERPROFILE"),
            };
            apply("HOME", home);
            apply("USERPROFILE", userprofile);
            saved
        }
    }

    impl Drop for HomeEnv {
        fn drop(&mut self) {
            match self.home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.userprofile.take() {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    fn apply(key: &str, value: Option<&str>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    /// The POSIX variable wins when both are set: a POSIX-emulating shell on
    /// Windows sets `HOME` on purpose.
    #[test]
    fn home_wins_over_userprofile() {
        let _guard = env_lock();
        let _env = HomeEnv::set(Some("/posix/home"), Some(r"C:\Users\win"));
        assert_eq!(home_dir(), Some(PathBuf::from("/posix/home")));
    }

    /// The Windows case this module exists for: no `HOME`, only `USERPROFILE`.
    #[test]
    fn userprofile_is_the_windows_fallback() {
        let _guard = env_lock();
        let _env = HomeEnv::set(None, Some(r"C:\Users\win"));
        assert_eq!(home_dir(), Some(PathBuf::from(r"C:\Users\win")));
    }

    /// An empty value is not a home directory — substituting it would resolve
    /// `~/x` to the filesystem root.
    #[test]
    fn an_empty_value_is_not_a_home() {
        let _guard = env_lock();
        let _env = HomeEnv::set(Some(""), None);
        assert_eq!(home_dir(), None);
    }

    #[test]
    fn no_home_variable_at_all() {
        let _guard = env_lock();
        let _env = HomeEnv::set(None, None);
        assert_eq!(home_dir(), None);
    }

    #[test]
    fn a_leading_tilde_expands() {
        let _guard = env_lock();
        let _env = HomeEnv::set(None, Some(r"C:\Users\win"));
        assert_eq!(
            expand_tilde("~/.codex/auth.json"),
            PathBuf::from(r"C:\Users\win").join(".codex/auth.json")
        );
    }

    #[test]
    fn a_bare_tilde_is_the_home_itself() {
        let _guard = env_lock();
        let _env = HomeEnv::set(Some("/posix/home"), None);
        assert_eq!(expand_tilde("~"), PathBuf::from("/posix/home"));
    }

    /// `~` is a legal filename character; only a leading one means "home", so
    /// a blanket replace would corrupt these paths.
    #[test]
    fn a_tilde_inside_the_path_is_an_ordinary_character() {
        let _guard = env_lock();
        let _env = HomeEnv::set(Some("/posix/home"), None);
        assert_eq!(
            expand_tilde("/var/cache/file~1"),
            PathBuf::from("/var/cache/file~1")
        );
    }

    /// Without a home there is nothing to expand against, so the path is left
    /// as written rather than silently rooted.
    #[test]
    fn a_tilde_without_a_home_is_left_alone() {
        let _guard = env_lock();
        let _env = HomeEnv::set(None, None);
        assert_eq!(expand_tilde("~/.codex"), PathBuf::from("~/.codex"));
    }

    /// On Windows a backslash is the native separator and `~\…` is as natural
    /// to type as `~/…`.
    #[cfg(windows)]
    #[test]
    fn a_backslash_tilde_prefix_expands_on_windows() {
        let _guard = env_lock();
        let _env = HomeEnv::set(None, Some(r"C:\Users\win"));
        assert_eq!(
            expand_tilde(r"~\.codex\auth.json"),
            PathBuf::from(r"C:\Users\win").join(r".codex\auth.json")
        );
    }
}
