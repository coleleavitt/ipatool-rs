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
    #[error("{}", join_messages(.0))]
    Joined(Vec<Error>),
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    /// Go's `fmt.Errorf("context: %w", err)` for an error already in hand.
    #[must_use]
    pub fn wrap(self, context: impl Display) -> Self {
        Self::Context {
            context: context.to_string(),
            source: Box::new(self),
        }
    }

    /// Go's `errors.Join`: `None` when every error is `None`, else the messages joined by newlines.
    pub fn join(errors: impl IntoIterator<Item = Option<Error>>) -> Option<Self> {
        let errors: Vec<Error> = errors.into_iter().flatten().collect();
        match errors.len() {
            0 => None,
            1 => errors.into_iter().next(),
            _ => Some(Self::Joined(errors)),
        }
    }
}

fn join_messages(errors: &[Error]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
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
