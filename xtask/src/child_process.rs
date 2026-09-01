use std::{io, process::Output, time::Duration};

use anyhow::{Context, Result, bail};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
    time::{self, Instant},
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn output(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<Output> {
    command
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut command = CommandWrap::from(command);
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);
    let mut child = command.spawn().context("spawn bounded child process")?;
    let stdout = child.stdout().take().context("open child stdout")?;
    let stderr = child.stderr().take().context("open child stderr")?;
    let stdout_reader = tokio::spawn(read_bounded(stdout, max_output_bytes));
    let stderr_reader = tokio::spawn(read_bounded(stderr, max_output_bytes));
    let deadline = Instant::now() + timeout;

    if let Some(input) = stdin {
        let mut child_stdin = child.stdin().take().context("open child stdin")?;
        let write = async move {
            child_stdin.write_all(&input).await?;
            child_stdin.shutdown().await
        };
        match time::timeout_at(deadline, write).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                terminate_and_reap(&mut child).await?;
                join_readers(stdout_reader, stderr_reader).await?;
                return Err(error).context("write child standard input");
            }
            Err(_) => {
                terminate_and_reap(&mut child).await?;
                join_readers(stdout_reader, stderr_reader).await?;
                bail!("child process timed out while reading standard input");
            }
        }
    }

    let (status, timed_out) = wait_until_exit(&mut child, deadline).await?;
    let ((stdout, stdout_truncated), (stderr, stderr_truncated)) =
        join_readers(stdout_reader, stderr_reader).await?;
    if stdout_truncated || stderr_truncated {
        bail!("child process output exceeded its bounded capture budget");
    }
    if timed_out {
        bail!("child process exceeded its execution timeout");
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn wait_until_exit(
    child: &mut Box<dyn ChildWrapper>,
    deadline: Instant,
) -> Result<(std::process::ExitStatus, bool)> {
    let mut timed_out = false;
    let mut cleanup_deadline = None;
    loop {
        if let Some(status) = child.try_wait().context("poll child process")? {
            return Ok((status, timed_out));
        }
        let now = Instant::now();
        if !timed_out && now >= deadline {
            child
                .start_kill()
                .context("terminate timed-out process group")?;
            timed_out = true;
            cleanup_deadline = Some(now + CLEANUP_TIMEOUT);
        }
        if cleanup_deadline.is_some_and(|cleanup_deadline| now >= cleanup_deadline) {
            bail!("timed-out process group did not exit within the cleanup deadline");
        }
        time::sleep(POLL_INTERVAL).await;
    }
}

async fn terminate_and_reap(child: &mut Box<dyn ChildWrapper>) -> Result<()> {
    child.start_kill().context("terminate process group")?;
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        if child
            .try_wait()
            .context("poll terminated process group")?
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("terminated process group did not exit within the cleanup deadline");
        }
        time::sleep(POLL_INTERVAL).await;
    }
}

async fn read_bounded<R>(mut reader: R, max_bytes: usize) -> io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut truncated = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok((output, truncated));
        }
        let remaining = max_bytes.saturating_sub(output.len());
        output.extend_from_slice(&chunk[..read.min(remaining)]);
        truncated |= read > remaining;
    }
}

async fn join_readers(
    mut stdout: JoinHandle<io::Result<(Vec<u8>, bool)>>,
    mut stderr: JoinHandle<io::Result<(Vec<u8>, bool)>>,
) -> Result<((Vec<u8>, bool), (Vec<u8>, bool))> {
    if let Ok(joined) = time::timeout(CLEANUP_TIMEOUT, async {
        tokio::try_join!(&mut stdout, &mut stderr)
    })
    .await
    {
        let (stdout, stderr) = joined.context("join child output readers")?;
        Ok((stdout?, stderr?))
    } else {
        stdout.abort();
        stderr.abort();
        bail!("child output readers did not stop within the cleanup deadline")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, time::SystemTime};

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timeout_kills_and_reaps_the_process_group() {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "agz-rust-coder-xtask-timeout-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create timeout fixture");
        let pid_file = root.join("child.pid");
        let mut command = Command::new("sh");
        command
            .current_dir(&root)
            .arg("-c")
            .arg("sleep 60 & echo $! > child.pid; wait");

        let result = output(command, None, Duration::from_millis(200), 4096).await;
        assert!(result.is_err());
        let child_pid = fs::read_to_string(&pid_file)
            .expect("child pid file")
            .trim()
            .parse::<u32>()
            .expect("child pid");
        assert!(
            !PathBuf::from(format!("/proc/{child_pid}")).exists(),
            "timed-out process-group child {child_pid} is still alive"
        );
        fs::remove_dir_all(root).expect("remove timeout fixture");
    }
}
