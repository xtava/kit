use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process,
    thread,
    time::Duration,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("command fixture failed: {error}");
        process::exit(96);
    }
}

fn run() -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let root = executable
        .parent()
        .ok_or_else(|| "fixture executable has no parent directory".to_owned())?;
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let invocation = claim_directory(&root.join("observed"))?;
    write_arguments(&invocation.join("arguments"), &arguments)?;
    fs::write(invocation.join("pid"), process::id().to_string())
        .map_err(|error| error.to_string())?;

    let rule = matching_rule(&root.join("rules"), &arguments)?.ok_or_else(|| {
        format!(
            "no rule matched arguments {:?}",
            arguments.iter().map(|argument| argument.to_string_lossy()).collect::<Vec<_>>()
        )
    })?;
    let response = claim_response(&rule.join("responses"))?
        .ok_or_else(|| "matching rule has no unused response".to_owned())?;
    fs::write(invocation.join("rule"), rule.file_name().unwrap_or_default().as_encoded_bytes())
        .map_err(|error| error.to_string())?;

    let mut stdin = Vec::new();
    io::stdin().read_to_end(&mut stdin).map_err(|error| error.to_string())?;
    fs::write(invocation.join("stdin"), &stdin).map_err(|error| error.to_string())?;
    fs::write(invocation.join("ready"), []).map_err(|error| error.to_string())?;
    let expected_stdin = response.join("expected-stdin");
    if expected_stdin.exists() {
        let expected = fs::read(expected_stdin).map_err(|error| error.to_string())?;
        if stdin != expected {
            return Err(format!(
                "stdin mismatch: expected {} bytes, received {} bytes",
                expected.len(),
                stdin.len()
            ));
        }
    }

    emit_events(&response.join("events"))?;
    let behavior = fs::read_to_string(response.join("behavior"))
        .map_err(|error| error.to_string())?;
    if behavior.trim() == "hang" {
        loop {
            thread::park_timeout(Duration::from_secs(3600));
        }
    }
    let code = behavior
        .trim()
        .strip_prefix("exit:")
        .ok_or_else(|| format!("unsupported fixture behavior {behavior:?}"))?
        .parse::<i32>()
        .map_err(|error| error.to_string())?;
    process::exit(code);
}

fn matching_rule(root: &Path, arguments: &[OsString]) -> Result<Option<PathBuf>, String> {
    let mut rules = sorted_directories(root)?;
    rules.sort();
    for rule in rules {
        let expected = read_arguments(&rule.join("arguments"))?;
        if expected.len() == arguments.len()
            && expected
                .iter()
                .zip(arguments)
                .all(|(expected, actual)| expected == &encode_os(actual))
        {
            return Ok(Some(rule));
        }
    }
    Ok(None)
}

fn claim_response(root: &Path) -> Result<Option<PathBuf>, String> {
    let mut responses = sorted_directories(root)?;
    responses.sort();
    for response in responses {
        match OpenOptions::new().write(true).create_new(true).open(response.join("claimed")) {
            Ok(_) => return Ok(Some(response)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(None)
}

fn emit_events(root: &Path) -> Result<(), String> {
    let mut events = sorted_directories(root)?;
    events.sort();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    for event in events {
        let delay_ms = fs::read_to_string(event.join("delay-ms"))
            .map_err(|error| error.to_string())?
            .trim()
            .parse::<u64>()
            .map_err(|error| error.to_string())?;
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
        let bytes = fs::read(event.join("bytes")).map_err(|error| error.to_string())?;
        match fs::read_to_string(event.join("stream"))
            .map_err(|error| error.to_string())?
            .trim()
        {
            "stdout" => {
                stdout.write_all(&bytes).map_err(|error| error.to_string())?;
                stdout.flush().map_err(|error| error.to_string())?;
            }
            "stderr" => {
                stderr.write_all(&bytes).map_err(|error| error.to_string())?;
                stderr.flush().map_err(|error| error.to_string())?;
            }
            stream => return Err(format!("unsupported output stream {stream:?}")),
        }
    }
    Ok(())
}

fn claim_directory(root: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    for index in 0..100_000u32 {
        let path = root.join(format!("{index:08}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("fixture invocation capacity exhausted".to_owned())
}

fn sorted_directories(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    fs::read_dir(root)
        .map_err(|error| error.to_string())?
        .map(|entry| entry.map_err(|error| error.to_string()))
        .filter_map(|entry| match entry {
            Ok(entry) if entry.path().is_dir() => Some(Ok(entry.path())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn write_arguments(root: &Path, arguments: &[OsString]) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    for (index, argument) in arguments.iter().enumerate() {
        fs::write(root.join(format!("{index:08}")), encode_os(argument))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn read_arguments(root: &Path) -> Result<Vec<Vec<u8>>, String> {
    let mut paths = sorted_files(root)?;
    paths.sort();
    paths.into_iter().map(|path| fs::read(path).map_err(|error| error.to_string())).collect()
}

fn sorted_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    fs::read_dir(root)
        .map_err(|error| error.to_string())?
        .map(|entry| entry.map_err(|error| error.to_string()))
        .filter_map(|entry| match entry {
            Ok(entry) if entry.path().is_file() => Some(Ok(entry.path())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

#[cfg(unix)]
fn encode_os(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn encode_os(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn encode_os(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}
