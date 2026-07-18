use super::{process::ProcessSupervisor, ConfigStore, Output, RepositoryLocator, Terminal};

/// The injected services, built once in `main` and borrowed into every [`super::Tool::run`].
pub struct Context {
    pub config: ConfigStore,
    pub out: Output,
    pub term: Terminal,
    pub repositories: RepositoryLocator,
    pub processes: ProcessSupervisor,
}
