use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicI32, Ordering},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

static SIGNAL: AtomicI32 = AtomicI32::new(0);
extern "C" fn on_signal(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::Relaxed);
}
pub fn received_signal() -> i32 {
    SIGNAL.load(Ordering::Relaxed)
}
pub struct SignalHandlers(Vec<(libc::c_int, libc::sigaction)>);
impl Drop for SignalHandlers {
    fn drop(&mut self) {
        for (signal, previous) in &self.0 {
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}
pub fn install_signal_handlers() -> Result<SignalHandlers> {
    let mut handlers = SignalHandlers(Vec::new());
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        handlers.0.push((signal, previous));
    }
    Ok(handlers)
}
fn check_signal() -> Result<()> {
    anyhow::ensure!(
        received_signal() == 0,
        "Interrupted by signal {}",
        received_signal()
    );
    Ok(())
}

struct Running {
    child: Child,
    grouped: bool,
    finished: bool,
}
impl Drop for Running {
    fn drop(&mut self) {
        if !self.finished {
            if self.grouped {
                // This process group was created for this command alone.
                unsafe {
                    libc::kill(-(self.child.id() as i32), libc::SIGKILL);
                }
            } else {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }
}

/// Drain stdout, stderr and stdin concurrently. The deadline covers both the
/// child and pipe-owning descendants; every error path reaps the child.
pub fn output(command: Command, input: Option<&str>, interactive: bool) -> Result<String> {
    output_with_timeout(command, input, interactive, Duration::from_secs(120))
}
fn output_with_timeout(
    mut command: Command,
    input: Option<&str>,
    interactive: bool,
    timeout: Duration,
) -> Result<String> {
    check_signal()?;
    command.stdout(Stdio::piped()).stderr(if interactive {
        Stdio::inherit()
    } else {
        Stdio::piped()
    });
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    if !interactive {
        command.process_group(0);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut running = Running {
        child: command
            .spawn()
            .with_context(|| format!("Starting {program}"))?,
        grouped: !interactive,
        finished: false,
    };
    let start = Instant::now();
    let drain = |mut stream: Box<dyn Read + Send>| {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stream.read_to_end(&mut bytes).map(|_| bytes);
            let _ = sender.send(result);
        });
        receiver
    };
    let out = drain(Box::new(running.child.stdout.take().unwrap()));
    let err = running
        .child
        .stderr
        .take()
        .map(|stream| drain(Box::new(stream)));
    let writer = input.map(|text| {
        let mut stdin = running.child.stdin.take().unwrap();
        let bytes = text.as_bytes().to_vec();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(stdin.write_all(&bytes));
        });
        receiver
    });
    let timeout_error = || {
        anyhow::anyhow!(
            "Command timed out after {} seconds: {program}",
            timeout.as_secs_f64()
        )
    };
    let status = loop {
        check_signal()?;
        if !interactive && start.elapsed() >= timeout {
            return Err(timeout_error());
        }
        if let Some(status) = running.child.wait_timeout(Duration::from_millis(100))? {
            break status;
        }
    };
    let receive = |receiver: mpsc::Receiver<std::io::Result<Vec<u8>>>| -> Result<Vec<u8>> {
        loop {
            check_signal()?;
            if !interactive && start.elapsed() >= timeout {
                return Err(timeout_error());
            }
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return Ok(result?),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(error) => return Err(error.into()),
            }
        }
    };
    let stdout = receive(out)?;
    let stderr = err.map(receive).transpose()?.unwrap_or_default();
    if let Some(writer) = writer {
        let written = if interactive {
            writer.recv()?
        } else {
            writer
                .recv_timeout(timeout.saturating_sub(start.elapsed()))
                .map_err(|_| timeout_error())?
        };
        if status.success() {
            written?;
        }
    }
    running.finished = true;
    if !status.success() {
        let error = String::from_utf8_lossy(&stderr).trim().to_owned();
        bail!(
            "{}",
            if error.is_empty() {
                format!("Command failed ({status}): {program}")
            } else {
                error
            }
        );
    }
    Ok(String::from_utf8(stdout)
        .context("Command returned non-UTF-8 output")?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    static SIGNAL_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn shell(script: &str) -> Command {
        let mut command = Command::new(option_env!("JJ_SYNC_SHELL").unwrap_or("/bin/sh"));
        command.args(["-c", script]);
        command
    }
    #[test]
    fn drains_large_output_and_reports_failure() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        let command = shell(
            "i=0; while [ $i -lt 4000 ]; do echo stdout; echo stderr >&2; i=$((i + 1)); done",
        );
        assert_eq!(output(command, None, false).unwrap().lines().count(), 4000);
        assert!(
            output(shell("echo specific-error >&2; exit 23"), None, false)
                .unwrap_err()
                .to_string()
                .contains("specific-error")
        );
    }
    #[test]
    fn timeout_covers_child_and_descendants_that_hold_pipes_open() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        for script in ["while :; do :; done", "(while :; do :; done) & exit 0"] {
            let start = Instant::now();
            assert!(
                output_with_timeout(shell(script), None, false, Duration::from_millis(100))
                    .unwrap_err()
                    .to_string()
                    .contains("timed out")
            );
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }

    #[test]
    fn termination_interrupts_wait_and_reaps_the_command_group() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        let _handlers = install_signal_handlers().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let pidfile = directory.path().join("pid");
        let command = shell(&format!(
            "echo $$ > {}; while :; do :; done",
            shell_words::quote(pidfile.to_str().unwrap())
        ));
        let sender = thread::spawn(|| {
            thread::sleep(Duration::from_millis(200));
            unsafe {
                libc::kill(libc::getpid(), libc::SIGTERM);
            }
        });
        let result = output(command, None, false);
        sender.join().unwrap();
        // Reset the flag before assertions so a failed assertion cannot poison
        // another test in this process.
        SIGNAL.store(0, Ordering::Relaxed);
        assert!(result.unwrap_err().to_string().contains("Interrupted"));
        let pid: i32 = std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
