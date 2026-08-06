use std::{collections::BTreeMap, path::Path, process::ExitStatus};

use crate::onepassword::{OpClient, OpEnvironment, OpError};

use super::config::Operation;

pub struct OpsRunner {
    client: OpClient,
}

impl OpsRunner {
    pub fn new(client: OpClient) -> Self {
        Self { client }
    }

    pub async fn run(
        &self,
        operation: &Operation,
        working_directory: &Path,
        environment: &OpEnvironment,
        public_environment: &BTreeMap<String, String>,
    ) -> Result<ExitStatus, OpError> {
        self.client
            .run_operation(
                &environment.references(),
                public_environment,
                working_directory,
                &operation.command.program,
                &operation.command.args,
            )
            .await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        io::Write as _,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::onepassword::parse_dotenv;
    use crate::tools::ops::config::{CommandSpec, OpsConfig, SCHEMA_VERSION};

    struct FakeOp {
        root: PathBuf,
        executable: PathBuf,
        trace: PathBuf,
        snapshot: PathBuf,
        env_path: PathBuf,
    }

    impl FakeOp {
        fn new(run_status: i32) -> Self {
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
exit {run_status}
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
            working_dir: None,
            env_file: PathBuf::from("production.env"),
            command: CommandSpec { program: "printf".to_owned(), args: vec!["scoped".to_owned()] },
            parameters: Vec::new(),
        }
    }

    fn unselected_operation() -> Operation {
        Operation {
            id: "server".to_owned(),
            working_dir: None,
            env_file: PathBuf::from("production.env"),
            command: CommandSpec { program: "server".to_owned(), args: Vec::new() },
            parameters: Vec::new(),
        }
    }

    #[tokio::test]
    async fn fake_op_proves_exact_args_scoping_and_ephemeral_cleanup() {
        let fake = FakeOp::new(0);
        let runner = OpsRunner::new(fake.client());
        let catalog =
            OpsConfig { version: SCHEMA_VERSION, ops: vec![operation(), unselected_operation()] };
        let selected = catalog.operation("marketing").unwrap();
        let environment = parse_dotenv("MARKETING_TOKEN=op://Deploy/marketing/token").unwrap();

        let status =
            runner.run(selected, Path::new("."), &environment, &BTreeMap::new()).await.unwrap();

        assert!(status.success());
        let env_path = PathBuf::from(std::fs::read_to_string(&fake.env_path).unwrap());
        let trace = std::fs::read_to_string(&fake.trace).unwrap();
        let snapshot = std::fs::read_to_string(&fake.snapshot).unwrap();
        let expected_run = format!("run\t--env-file={}\t--\tprintf\tscoped", env_path.display());
        assert_eq!(trace.lines().collect::<Vec<_>>(), [expected_run.as_str()]);
        assert!(!trace.contains("--no-masking"));
        assert_eq!(snapshot, "MARKETING_TOKEN=op://Deploy/marketing/token\n");
        assert!(!snapshot.contains("SERVER_TOKEN"));
        assert!(!snapshot.contains("op://Deploy/server/token"));
        assert!(!env_path.exists());
    }

    #[tokio::test]
    async fn nonzero_child_status_is_returned_without_extra_op_calls() {
        let fake = FakeOp::new(72);
        let runner = OpsRunner::new(fake.client());
        let environment = parse_dotenv("MARKETING_TOKEN=op://Deploy/marketing/token").unwrap();

        let status =
            runner.run(&operation(), Path::new("."), &environment, &BTreeMap::new()).await.unwrap();
        let trace = std::fs::read_to_string(&fake.trace).unwrap();
        let env_path = PathBuf::from(std::fs::read_to_string(&fake.env_path).unwrap());
        let expected_run = format!("run\t--env-file={}\t--\tprintf\tscoped", env_path.display());

        assert_eq!(status.code(), Some(72));
        assert_eq!(trace.lines().collect::<Vec<_>>(), [expected_run.as_str()]);
    }
}
