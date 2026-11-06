use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("keychain error: {0}")]
    Keychain(String),
    #[error("value of {len} bytes does not fit the `security -i` stdin limit ({limit} bytes); refusing to fall back to argv")]
    TooLarge { len: usize, limit: usize },
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("lock busy: {0}")]
    LockBusy(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("refusing to write through symlinked folder {0}")]
    SymlinkedParent(PathBuf),
    #[error("{0} is a symlink (a dotfile manager such as stow or chezmoi may own it). agent-switch will not replace it with a regular file. Make it a regular file, or exclude it from the manager, then run the command again")]
    SymlinkedFile(PathBuf),
    /// The app answered a quit request with its own "still working" dialog and the user (or the script)
    /// cancelled it. `exited` lists the app processes that were already gone when the veto came.
    #[error("{app} is asking whether to interrupt its work. Answer its dialog (Cancel keeps it working), then run the command again")]
    QuitVetoed { app: &'static str, exited: Vec<u32> },
}

impl Error {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io { context: context.into(), source }
    }
}
