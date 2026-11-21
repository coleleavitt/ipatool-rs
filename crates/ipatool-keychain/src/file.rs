use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::item::StoredItem;
use crate::{Error, PassphrasePrompt, Result, jwe};

/// Go's `fileKeyring`: one JWE per key, file name percent-encoded (`%` and `/`).
pub(crate) struct FileKeyring {
    dir: PathBuf,
    prompt: PassphrasePrompt,
    passphrase: OnceLock<String>,
}

impl FileKeyring {
    pub fn new(dir: PathBuf, prompt: PassphrasePrompt) -> Self {
        Self {
            dir,
            prompt,
            passphrase: OnceLock::new(),
        }
    }

    fn dir(&self) -> Result<&Path> {
        match fs::metadata(&self.dir) {
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.recursive(true);
                // Go's `os.MkdirAll(dir, 0700)`; Windows has no mode bits.
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
                builder.create(&self.dir)?;
            }
            Err(err) => return Err(err.into()),
            Ok(meta) if !meta.is_dir() => {
                return Err(std::io::Error::other(format!(
                    "{} is a file, not a directory",
                    self.dir.display()
                ))
                .into());
            }
            Ok(_) => {}
        }
        Ok(&self.dir)
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        Ok(self.dir()?.join(escape(key)))
    }

    fn passphrase(&self) -> Result<&str> {
        if let Some(passphrase) = self.passphrase.get() {
            return Ok(passphrase);
        }
        let passphrase = (self.prompt)(self.dir()?).map_err(Error::Passphrase)?;
        Ok(self.passphrase.get_or_init(|| passphrase))
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>> {
        let token = match fs::read_to_string(self.path(key)?) {
            Err(err) if err.kind() == ErrorKind::NotFound => return Err(Error::NotFound),
            other => other?,
        };
        let payload = jwe::decrypt(&token, self.passphrase()?)?;
        Ok(serde_json::from_slice::<StoredItem>(&payload)?.data)
    }

    pub fn set(&self, item: &StoredItem) -> Result<()> {
        let payload = serde_json::to_vec(item)?;
        let created = time::OffsetDateTime::now_utc().to_string();
        let token = jwe::encrypt(&payload, self.passphrase()?, &created)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(self.path(&item.key)?)?;
        file.write_all(token.as_bytes())?;
        Ok(())
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        match fs::remove_file(self.path(key)?) {
            Err(err) if err.kind() == ErrorKind::NotFound => Err(Error::NotFound),
            other => Ok(other?),
        }
    }
}

fn escape(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for ch in key.chars() {
        if matches!(ch, '%' | '/') {
            write!(out, "%{:02X}", u32::from(ch)).expect("writing to a String cannot fail");
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(dir: &Path) -> FileKeyring {
        FileKeyring::new(
            dir.to_path_buf(),
            Box::new(|_| Ok("test-passphrase".to_owned())),
        )
    }

    #[test]
    fn escapes_like_go() {
        assert_eq!(escape("a/b%c"), "a%2Fb%25c");
    }

    #[test]
    fn stores_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let ring = ring(dir.path());
        assert!(matches!(ring.get("account"), Err(Error::NotFound)));
        ring.set(&StoredItem::new("account", b"data", "svc"))
            .unwrap();
        assert_eq!(ring.get("account").unwrap(), b"data");
        #[cfg(unix)]
        {
            let mode = fs::metadata(dir.path().join("account"))
                .unwrap()
                .permissions();
            assert_eq!(
                std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
                0o600
            );
        }
        ring.remove("account").unwrap();
        assert!(matches!(ring.remove("account"), Err(Error::NotFound)));
    }
}
