use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

use crate::tools::secrets::op::{OpClient, OpError, OpRunRequest, OpRunStatus, SecretReference};

use super::config::Operation;

const ENV_FILE_ATTEMPTS: usize = 32;
static NEXT_ENV_FILE: AtomicU64 = AtomicU64::new(0);

pub struct OpsRunner {
    client: OpClient,
}

impl OpsRunner {
    pub fn new(client: OpClient) -> Self {
        Self { client }
    }

    pub async fn run(&self, operation: &Operation) -> Result<OpRunStatus, RunnerError> {
        for reference in operation.refs.values() {
            self.client.preflight_reference(reference).await?;
        }

        let env_file = ScopedEnvFile::create(&operation.refs)?;
        let request =
            OpRunRequest::new(env_file.path(), &operation.command.program, &operation.command.args);
        self.client.run_operation(request).await.map_err(RunnerError::from)
    }
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error(transparent)]
    Op(#[from] OpError),
    #[error("create ephemeral ops reference file {}: {source}", path.display())]
    CreateEnvFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write ephemeral ops reference file {}: {source}", path.display())]
    WriteEnvFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not allocate a unique ephemeral ops reference file")]
    AllocateEnvFile,
}

struct ScopedEnvFile {
    path: PathBuf,
}

impl ScopedEnvFile {
    fn create(refs: &BTreeMap<String, SecretReference>) -> Result<Self, RunnerError> {
        for _ in 0..ENV_FILE_ATTEMPTS {
            let nonce = NEXT_ENV_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kit-ops-refs-{}-{nonce}.env", std::process::id()));
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(RunnerError::CreateEnvFile { path, source }),
            };
            let write_result = (|| {
                for (name, reference) in refs {
                    writeln!(file, "{name}={}", reference.as_str())?;
                }
                file.sync_all()
            })();
            if let Err(source) = write_result {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(RunnerError::WriteEnvFile { path, source });
            }
            return Ok(Self { path });
        }
        Err(RunnerError::AllocateEnvFile)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for ScopedEnvFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ScopedEnvFile").field("path", &self.path).finish()
    }
}

impl Drop for ScopedEnvFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        io::Write as _,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::tools::ops::config::{CommandSpec, OpsConfig, SCHEMA_VERSION};

    struct FakeOp {
        root: PathBuf,
        executable: PathBuf,
        trace: PathBuf,
        snapshot: PathBuf,
        env_path: PathBuf,
    }

    impl FakeOp {
        fn new(read_status: i32) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("kit-ops-fake-op-{}-{id}", std::process::id()));
            std::fs::create_dir(&root).unwrap();
            let executable = root.join("op");
            let trace = root.join("trace");
            let snapshot = root.join("env.snapshot");
            let env_path = root.join("env.path");
            let script = format!(
                r#"#!/bin/sh
trace='{trace}'
snapshot='{snapshot}'
env_path_record='{env_path}'
command=$1
printf '%s' "$1" >> "$trace"
shift
for argument in "$@"; do
  printf '\t%s' "$argument" >> "$trace"
done
printf '\n' >> "$trace"
if [ "$command" = read ]; then
  exit {read_status}
fi
[ "$command" = run ] || exit 90
case "$1" in
  --env-file=*) refs_file=${{1#--env-file=}} ;;
  *) exit 91 ;;
esac
[ "$2" = -- ] || exit 92
[ "$3" = printf ] || exit 93
[ "$4" = scoped ] || exit 94
cp "$refs_file" "$snapshot" || exit 95
printf '%s' "$refs_file" > "$env_path_record"
"#,
                trace = trace.display(),
                snapshot = snapshot.display(),
                env_path = env_path.display(),
            );
            let mut file = std::fs::File::create(&executable).unwrap();
            file.write_all(script.as_bytes()).unwrap();
            file.sync_all().unwrap();
            drop(file);
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&executable, permissions).unwrap();
            Self { root, executable, trace, snapshot, env_path }
        }

        fn client(&self) -> OpClient {
            OpClient::with_executable(self.executable.clone())
        }
    }

    impl Drop for FakeOp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn operation() -> Operation {
        Operation {
            id: "marketing".to_owned(),
            command: CommandSpec { program: "printf".to_owned(), args: vec!["scoped".to_owned()] },
            refs: BTreeMap::from([(
                "MARKETING_TOKEN".to_owned(),
                SecretReference::new("op://Deploy/marketing/token".to_owned()).unwrap(),
            )]),
        }
    }

    fn unselected_operation() -> Operation {
        Operation {
            id: "server".to_owned(),
            command: CommandSpec { program: "server".to_owned(), args: Vec::new() },
            refs: BTreeMap::from([(
                "SERVER_TOKEN".to_owned(),
                SecretReference::new("op://Deploy/server/token".to_owned()).unwrap(),
            )]),
        }
    }

    #[tokio::test]
    async fn fake_op_proves_exact_args_scoping_and_ephemeral_cleanup() {
        let fake = FakeOp::new(0);
        let runner = OpsRunner::new(fake.client());
        let catalog =
            OpsConfig { version: SCHEMA_VERSION, ops: vec![operation(), unselected_operation()] };
        let selected = catalog.operation("marketing").unwrap();

        let status = runner.run(selected).await.unwrap();

        assert!(status.success());
        let env_path = PathBuf::from(std::fs::read_to_string(&fake.env_path).unwrap());
        let trace = std::fs::read_to_string(&fake.trace).unwrap();
        let snapshot = std::fs::read_to_string(&fake.snapshot).unwrap();
        let expected_run = format!("run\t--env-file={}\t--\tprintf\tscoped", env_path.display());
        assert_eq!(
            trace.lines().collect::<Vec<_>>(),
            ["read\top://Deploy/marketing/token\t--no-newline\t--no-color", expected_run.as_str(),]
        );
        assert!(!trace.contains("--no-masking"));
        assert_eq!(snapshot, "MARKETING_TOKEN=op://Deploy/marketing/token\n");
        assert!(!snapshot.contains("SERVER_TOKEN"));
        assert!(!snapshot.contains("op://Deploy/server/token"));
        assert!(!env_path.exists());
    }

    #[tokio::test]
    async fn failed_preflight_names_the_ref_and_never_runs_the_command() {
        let fake = FakeOp::new(72);
        let runner = OpsRunner::new(fake.client());

        let error = runner.run(&operation()).await.expect_err("preflight must fail");
        let trace = std::fs::read_to_string(&fake.trace).unwrap();

        assert!(error.to_string().contains("op://Deploy/marketing/token"));
        assert_eq!(
            trace.lines().collect::<Vec<_>>(),
            ["read\top://Deploy/marketing/token\t--no-newline\t--no-color"]
        );
    }
}
