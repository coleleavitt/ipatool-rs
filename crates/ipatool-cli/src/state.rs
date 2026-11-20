//! State directory, as Go's `prepareStateDirectory` (`cmd/state_directory.go`):
//! `$XDG_STATE_HOME/ipatool`, else `$XDG_DATA_HOME/ipatool` (absolute values only),
//! else `~/.ipatool`. A legacy `~/.ipatool` is moved to the XDG path only when that
//! path does not exist yet and the rename works; otherwise the legacy path stays in use.

use std::fs::{self, DirBuilder};
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

const LEGACY_DIRECTORY: &str = ".ipatool";

pub fn prepare(home: &Path, env: impl Fn(&str) -> Option<String>) -> io::Result<PathBuf> {
    let legacy = home.join(LEGACY_DIRECTORY);
    let state = ["XDG_STATE_HOME", "XDG_DATA_HOME"]
        .into_iter()
        .filter_map(&env)
        .map(PathBuf::from)
        .find(|base| base.is_absolute())
        .map_or_else(|| legacy.clone(), |base| base.join("ipatool"));

    match fs::metadata(&legacy) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(io::Error::other(format!(
                    "legacy state path is not a directory: {}",
                    legacy.display()
                )));
            }
            if state.starts_with(&legacy) || fs::symlink_metadata(&state).is_ok() {
                return Ok(legacy);
            }
            let moved = state
                .parent()
                .is_none_or(|parent| create_dir(parent).is_ok())
                && fs::rename(&legacy, &state).is_ok();
            Ok(if moved { state } else { legacy })
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            create_dir(&state).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("failed to create state directory: {err}"),
                )
            })?;
            Ok(state)
        }
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("could not read legacy state directory metadata: {err}"),
        )),
    }
}

fn create_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn uses_legacy_without_xdg() {
        let home = tempfile::tempdir().unwrap();
        let dir = prepare(home.path(), env(&[])).unwrap();
        assert_eq!(dir, home.path().join(".ipatool"));
        assert!(dir.is_dir());
    }

    #[test]
    fn prefers_state_home_and_ignores_relative() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        let vars = [
            ("XDG_STATE_HOME", "relative".to_owned()),
            ("XDG_DATA_HOME", data.display().to_string()),
        ];
        assert_eq!(
            prepare(home.path(), env(&vars)).unwrap(),
            data.join("ipatool")
        );
    }

    #[test]
    fn migrates_legacy_once() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".ipatool")).unwrap();
        fs::write(home.path().join(".ipatool/cookies"), "[]").unwrap();
        let state = home.path().join("state");
        let vars = [("XDG_STATE_HOME", state.display().to_string())];
        let dir = prepare(home.path(), env(&vars)).unwrap();
        assert_eq!(dir, state.join("ipatool"));
        assert!(dir.join("cookies").exists());
        assert!(!home.path().join(".ipatool").exists());

        // Both exist: keep the legacy directory and never merge.
        fs::create_dir(home.path().join(".ipatool")).unwrap();
        assert_eq!(
            prepare(home.path(), env(&vars)).unwrap(),
            home.path().join(".ipatool")
        );
    }
}
