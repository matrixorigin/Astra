//! Ordered, non-blocking file effects for TUI-owned projections.

use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

const WRITE_QUEUE_CAPACITY: usize = 512;
const WRITER_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TuiFileWriteError {
    pub target: &'static str,
    pub path: PathBuf,
    pub message: String,
}

impl TuiFileWriteError {
    pub(crate) fn user_message(&self) -> String {
        format!(
            "{} could not be saved at {}: {}",
            self.target,
            self.path.display(),
            self.message
        )
    }
}

enum WriteCommand {
    AppendLine {
        target: &'static str,
        path: PathBuf,
        line: String,
    },
    RewriteLines {
        target: &'static str,
        path: PathBuf,
        lines: Vec<String>,
    },
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Clone)]
pub(crate) struct TuiFileWriter {
    tx: mpsc::Sender<WriteCommand>,
    error_tx: mpsc::UnboundedSender<TuiFileWriteError>,
}

pub(crate) struct TuiFileWriterRuntime {
    tx: mpsc::Sender<WriteCommand>,
    join: tokio::task::JoinHandle<()>,
}

pub(crate) fn spawn() -> (
    TuiFileWriter,
    TuiFileWriterRuntime,
    mpsc::UnboundedReceiver<TuiFileWriteError>,
) {
    let (tx, rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
    let (error_tx, error_rx) = mpsc::unbounded_channel();
    let join = tokio::spawn(run_writer(rx, error_tx.clone()));
    (
        TuiFileWriter {
            tx: tx.clone(),
            error_tx,
        },
        TuiFileWriterRuntime { tx, join },
        error_rx,
    )
}

impl TuiFileWriter {
    pub(crate) fn append_line(&self, target: &'static str, path: PathBuf, line: String) {
        let error_path = path.clone();
        self.try_enqueue(
            target,
            error_path,
            WriteCommand::AppendLine { target, path, line },
        );
    }

    pub(crate) fn rewrite_lines(&self, target: &'static str, path: PathBuf, lines: Vec<String>) {
        let error_path = path.clone();
        self.try_enqueue(
            target,
            error_path,
            WriteCommand::RewriteLines {
                target,
                path,
                lines,
            },
        );
    }

    fn try_enqueue(&self, target: &'static str, path: PathBuf, command: WriteCommand) {
        if let Err(error) = self.tx.try_send(command) {
            let message = match error {
                mpsc::error::TrySendError::Full(_) => {
                    format!("write queue is full ({WRITE_QUEUE_CAPACITY} pending operations)")
                }
                mpsc::error::TrySendError::Closed(_) => "write worker is unavailable".to_string(),
            };
            let _ = self.error_tx.send(TuiFileWriteError {
                target,
                path,
                message,
            });
        }
    }

    pub(crate) async fn flush(&self) -> Result<(), String> {
        tokio::time::timeout(WRITER_CONTROL_TIMEOUT, async {
            let (ack_tx, ack_rx) = oneshot::channel();
            self.tx
                .send(WriteCommand::Flush(ack_tx))
                .await
                .map_err(|_| "TUI file writer is unavailable".to_string())?;
            ack_rx
                .await
                .map_err(|_| "TUI file writer stopped before flush completed".to_string())
        })
        .await
        .map_err(|_| "TUI file writer flush timed out after 2s".to_string())?
    }
}

impl TuiFileWriterRuntime {
    pub(crate) async fn shutdown(self) -> Result<(), String> {
        tokio::time::timeout(WRITER_CONTROL_TIMEOUT, async {
            let (ack_tx, ack_rx) = oneshot::channel();
            self.tx
                .send(WriteCommand::Shutdown(ack_tx))
                .await
                .map_err(|_| "TUI file writer is unavailable".to_string())?;
            ack_rx
                .await
                .map_err(|_| "TUI file writer stopped before shutdown completed".to_string())?;
            self.join
                .await
                .map_err(|error| format!("TUI file writer task failed: {error}"))
        })
        .await
        .map_err(|_| "TUI file writer shutdown timed out after 2s".to_string())?
    }
}

async fn run_writer(
    mut rx: mpsc::Receiver<WriteCommand>,
    error_tx: mpsc::UnboundedSender<TuiFileWriteError>,
) {
    while let Some(command) = rx.recv().await {
        match command {
            WriteCommand::AppendLine { target, path, line } => {
                if let Err(error) = append_line(&path, &line).await {
                    report_error(&error_tx, target, path, error);
                }
            }
            WriteCommand::RewriteLines {
                target,
                path,
                lines,
            } => {
                if let Err(error) = rewrite_lines(&path, &lines).await {
                    report_error(&error_tx, target, path, error);
                }
            }
            WriteCommand::Flush(ack) => {
                let _ = ack.send(());
            }
            WriteCommand::Shutdown(ack) => {
                let _ = ack.send(());
                break;
            }
        }
    }
}

fn report_error(
    error_tx: &mpsc::UnboundedSender<TuiFileWriteError>,
    target: &'static str,
    path: PathBuf,
    error: std::io::Error,
) {
    report_message(error_tx, target, path, error.to_string());
}

fn report_message(
    error_tx: &mpsc::UnboundedSender<TuiFileWriteError>,
    target: &'static str,
    path: PathBuf,
    message: String,
) {
    tracing::warn!(target, path = %path.display(), %message, "TUI file write failed");
    let _ = error_tx.send(TuiFileWriteError {
        target,
        path,
        message,
    });
}

async fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    file.flush().await
}

async fn rewrite_lines(path: &Path, lines: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    tokio::fs::write(path, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_writes_preserve_order_and_flush_before_read() {
        let dir = crate::tests::test_temp_dir();
        let path = dir.path().join("ordered.log");
        let (writer, runtime, mut errors) = spawn();

        writer.append_line("test", path.clone(), "one".into());
        writer.append_line("test", path.clone(), "two".into());
        writer.append_line("test", path.clone(), "three".into());
        writer.flush().await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "one\ntwo\nthree\n"
        );
        assert!(errors.try_recv().is_err());
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rewrite_is_ordered_after_prior_appends() {
        let dir = crate::tests::test_temp_dir();
        let path = dir.path().join("history");
        let (writer, runtime, _errors) = spawn();

        writer.append_line("test", path.clone(), "old".into());
        writer.rewrite_lines("test", path.clone(), vec!["new-1".into(), "new-2".into()]);
        writer.flush().await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "new-1\nnew-2\n"
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_drains_all_prior_writes() {
        let dir = crate::tests::test_temp_dir();
        let path = dir.path().join("shutdown.log");
        let (writer, runtime, _errors) = spawn();
        writer.append_line("test", path.clone(), "before shutdown".into());

        runtime.shutdown().await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(path).await.unwrap(),
            "before shutdown\n"
        );
    }

    #[tokio::test]
    async fn write_failures_are_observable_without_stopping_later_commands() {
        let dir = crate::tests::test_temp_dir();
        let bad_parent = dir.path().join("not-a-directory");
        tokio::fs::write(&bad_parent, "file").await.unwrap();
        let good_path = dir.path().join("good.log");
        let (writer, runtime, mut errors) = spawn();

        writer.append_line("transcript", bad_parent.join("bad.log"), "lost".into());
        writer.append_line("history", good_path.clone(), "kept".into());
        writer.flush().await.unwrap();

        let error = errors.try_recv().expect("write error must be observable");
        assert_eq!(error.target, "transcript");
        assert_eq!(
            tokio::fs::read_to_string(good_path).await.unwrap(),
            "kept\n"
        );
        runtime.shutdown().await.unwrap();
    }
}
