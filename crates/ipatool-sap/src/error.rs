use std::fmt::Display;

/// Errors carry Go's messages so both tools report the same text.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<Error>,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `fmt.Errorf("context: %w", err)` for results.
pub trait Context<T> {
    fn context(self, context: impl Display) -> Result<T>;
    fn with_context<C: Display>(self, context: impl FnOnce() -> C) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context(self, context: impl Display) -> Result<T> {
        self.map_err(|source| Error::Context {
            context: context.to_string(),
            source: Box::new(source.into()),
        })
    }

    fn with_context<C: Display>(self, context: impl FnOnce() -> C) -> Result<T> {
        self.map_err(|source| Error::Context {
            context: context().to_string(),
            source: Box::new(source.into()),
        })
    }
}
