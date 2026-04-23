use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing_subscriber::fmt::MakeWriter;

const MB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostLoggingConfig {
    pub level: String,
    pub max_file_size_mb: u64,
    pub max_files: usize,
    pub max_age_days: u64,
}

impl Default for HostLoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            max_file_size_mb: 8,
            max_files: 5,
            max_age_days: 7,
        }
    }
}

#[derive(Debug)]
struct RotatingFileWriter {
    dir: PathBuf,
    base_name: String,
    config: HostLoggingConfig,
    file: File,
}

impl RotatingFileWriter {
    fn new(path: PathBuf, config: HostLoggingConfig) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            dir: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            base_name: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("host.log")
                .to_string(),
            config,
            file,
        })
    }

    fn max_bytes(&self) -> u64 {
        self.config.max_file_size_mb.saturating_mul(MB).max(MB)
    }

    fn rotate_if_needed(&mut self, incoming_len: usize) -> io::Result<()> {
        let current_size = self.file.metadata()?.len();
        if current_size.saturating_add(incoming_len as u64) < self.max_bytes() {
            return Ok(());
        }

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let current_path = self.dir.join(&self.base_name);
        let rotated_path = self.dir.join(format!("{}.{}", self.base_name, ts));
        let placeholder_path = self.dir.join(format!("{}.active", self.base_name));

        self.file.flush()?;
        let placeholder = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&placeholder_path)?;
        let old_file = std::mem::replace(&mut self.file, placeholder);
        drop(old_file);
        if current_path.exists() {
            let _ = fs::copy(&current_path, &rotated_path)?;
        }
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&current_path)?;
        let _ = fs::remove_file(&placeholder_path);

        self.cleanup_archives()
    }

    fn cleanup_archives(&mut self) -> io::Result<()> {
        let cutoff = if self.config.max_age_days == 0 {
            None
        } else {
            Some(
                SystemTime::now()
                    .checked_sub(Duration::from_secs(
                        self.config.max_age_days.saturating_mul(24 * 60 * 60),
                    ))
                    .unwrap_or(UNIX_EPOCH),
            )
        };

        let prefix = format!("{}.", self.base_name);
        let mut archives = Vec::new();

        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(name) => name,
                None => continue,
            };
            if !name.starts_with(&prefix) {
                continue;
            }

            let metadata = entry.metadata()?;
            if let Some(cutoff) = cutoff {
                if metadata.modified().ok().is_some_and(|m| m < cutoff) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
            }
            archives.push((metadata.modified().unwrap_or(UNIX_EPOCH), path));
        }

        archives.sort_by_key(|(modified, _)| *modified);
        while archives.len() > self.config.max_files {
            if let Some((_, path)) = archives.first() {
                let _ = fs::remove_file(path);
            }
            archives.remove(0);
        }

        Ok(())
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(buf.len())?;
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Clone, Debug)]
pub struct SharedRotatingWriter {
    inner: Arc<Mutex<RotatingFileWriter>>,
}

impl SharedRotatingWriter {
    pub fn new(path: PathBuf, config: HostLoggingConfig) -> io::Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(RotatingFileWriter::new(path, config)?)),
        })
    }
}

pub struct SharedWriterGuard {
    inner: Arc<Mutex<RotatingFileWriter>>,
}

impl Write for SharedWriterGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log writer poisoned"))?;
        guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log writer poisoned"))?;
        guard.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedRotatingWriter {
    type Writer = SharedWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriterGuard {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log_dir(name: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("wsl-memory-{}-{}", name, ts))
    }

    #[test]
    fn rotates_and_prunes_archives() {
        let dir = temp_log_dir("rotate");
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("host.log");
        let config = HostLoggingConfig {
            max_file_size_mb: 1,
            max_files: 1,
            max_age_days: 7,
            ..HostLoggingConfig::default()
        };
        let mut writer = RotatingFileWriter::new(path.clone(), config).expect("writer");
        let big = vec![b'x'; (MB as usize) + 128];
        writer.write_all(&big).expect("write one");
        writer.write_all(&big).expect("write two");
        writer.flush().expect("flush");

        let entries: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").path())
            .collect();
        let archives = entries
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.starts_with("host.log."))
            })
            .count();
        assert_eq!(archives, 1, "should keep only one archive");
        assert!(path.exists(), "current log file should exist");

        let _ = fs::remove_dir_all(dir);
    }
}
