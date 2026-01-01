use anyhow::{Context, Result};
use dashmap::DashMap;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::signal;
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio::time;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Deserialize)]
struct Config {
    torrent_list: String,
    download: DownloadConfig,
    archive: ArchiveConfig,
    state: StateConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct DownloadConfig {
    max_concurrent: usize,
    download_dir: String,
    timeout: u64,
    max_download_speed: Option<String>,
    max_upload_speed: Option<String>,
    seed_ratio: Option<f32>,
    bt_tracker: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ArchiveConfig {
    enabled: bool,
    archive_dir: String,
    compression_level: u8,
    delete_after_archive: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct StateConfig {
    state_file: String,
    save_interval: u64,
}

// Flexible torrent list - supports both string arrays and object arrays
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TorrentItem {
    Simple(String),
    Detailed(DetailedTorrent),
}

#[derive(Debug, Clone, Deserialize)]
struct DetailedTorrent {
    #[serde(alias = "link")]
    magnet: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentList {
    torrents: Vec<TorrentItem>,
}

impl TorrentItem {
    fn into_link(self) -> String {
        match self {
            TorrentItem::Simple(s) => s,
            TorrentItem::Detailed(d) => d.magnet,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum TorrentStatus {
    Pending,
    Downloading,
    Downloaded,
    Archiving,
    Archived,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TorrentState {
    name: String,
    link: String,
    status: TorrentStatus,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct AppState {
    torrents: Vec<TorrentState>,
}

struct App {
    config: Config,
    state: Arc<RwLock<AppState>>,
    status_map: Arc<DashMap<String, TorrentStatus>>,
    shutdown: Arc<AtomicBool>,
    // Progress tracking
    total_torrents: Arc<AtomicUsize>,
    completed_torrents: Arc<AtomicUsize>,
    completed_bytes: Arc<AtomicU64>,
    // Track in-progress downloads: link -> current bytes
    active_downloads: Arc<DashMap<String, u64>>,
}

impl App {
    fn new(config: Config) -> Result<Self> {
        let state = Self::load_state(&config.state.state_file)?;
        let status_map = Arc::new(DashMap::new());

        for torrent in &state.torrents {
            status_map.insert(torrent.link.clone(), torrent.status.clone());
        }

        Ok(Self {
            config,
            state: Arc::new(RwLock::new(state)),
            status_map,
            shutdown: Arc::new(AtomicBool::new(false)),
            total_torrents: Arc::new(AtomicUsize::new(0)),
            completed_torrents: Arc::new(AtomicUsize::new(0)),
            completed_bytes: Arc::new(AtomicU64::new(0)),
            active_downloads: Arc::new(DashMap::new()),
        })
    }

    fn get_total_bytes(&self) -> u64 {
        let completed = self.completed_bytes.load(Ordering::Relaxed);
        let active: u64 = self.active_downloads.iter().map(|e| *e.value()).sum();
        completed + active
    }

    fn format_bytes(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        const TB: u64 = GB * 1024;

        if bytes >= TB {
            format!("{:.2} TB", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.2} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.2} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.2} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} B", bytes)
        }
    }

    fn load_state(path: &str) -> Result<AppState> {
        if Path::new(path).exists() {
            let content = fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content).unwrap_or_default())
        } else {
            Ok(AppState::default())
        }
    }

    async fn save_state(&self) -> Result<()> {
        let state = self.state.read().await;
        let content = serde_json::to_string_pretty(&*state)?;
        fs::write(&self.config.state.state_file, content)?;
        Ok(())
    }

    async fn check_aria2c() -> Result<()> {
        let output = Command::new("aria2c")
            .arg("--version")
            .output()
            .await
            .context("aria2c not found. Please install aria2c:\n  Windows: winget install aria2\n  Linux: sudo apt install aria2\n  macOS: brew install aria2")?;

        if !output.status.success() {
            anyhow::bail!("aria2c check failed");
        }

        let version = String::from_utf8_lossy(&output.stdout);
        let first_line = version.lines().next().unwrap_or("unknown");
        info!("Found {}", first_line);
        Ok(())
    }

    async fn run(&self) -> Result<()> {
        // Check aria2c is available
        Self::check_aria2c().await?;

        // Create directories
        fs::create_dir_all(&self.config.download.download_dir)?;
        if self.config.archive.enabled {
            fs::create_dir_all(&self.config.archive.archive_dir)?;
        }

        // Load torrent list
        let torrent_list: TorrentList = {
            let content = fs::read_to_string(&self.config.torrent_list)
                .context("Failed to read torrent list")?;
            serde_json::from_str(&content)?
        };

        let links: Vec<String> = torrent_list
            .torrents
            .into_iter()
            .map(|item| item.into_link())
            .collect();

        // Count pending torrents
        let pending_count = links.iter().filter(|link| {
            self.status_map.get(*link).map(|s| *s != TorrentStatus::Downloaded && *s != TorrentStatus::Archived).unwrap_or(true)
        }).count();

        self.total_torrents.store(pending_count, Ordering::Relaxed);
        info!("Loaded {} torrents ({} pending)", links.len(), pending_count);

        // Initialize state for new torrents
        {
            let mut state = self.state.write().await;
            for link in &links {
                if !state.torrents.iter().any(|t| &t.link == link) {
                    let name = Self::extract_name(link);
                    state.torrents.push(TorrentState {
                        name,
                        link: link.clone(),
                        status: TorrentStatus::Pending,
                        error: None,
                    });
                    self.status_map.insert(link.clone(), TorrentStatus::Pending);
                }
            }
        }
        self.save_state().await?;

        // Create progress bar
        let progress_bar = ProgressBar::new(pending_count as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} torrents | {msg}")
                .unwrap()
                .progress_chars("█▓▒░  ")
        );
        progress_bar.set_message("0 B downloaded");

        // Start progress bar updater
        let pb_clone = progress_bar.clone();
        let completed = self.completed_torrents.clone();
        let completed_bytes = self.completed_bytes.clone();
        let active_downloads = self.active_downloads.clone();
        let shutdown_pb = self.shutdown.clone();
        let progress_updater = tokio::spawn(async move {
            loop {
                if shutdown_pb.load(Ordering::Relaxed) {
                    break;
                }
                let done = completed.load(Ordering::Relaxed);
                let completed_b = completed_bytes.load(Ordering::Relaxed);
                let active_b: u64 = active_downloads.iter().map(|e| *e.value()).sum();
                let total_b = completed_b + active_b;
                pb_clone.set_position(done as u64);
                pb_clone.set_message(format!("{} downloaded", Self::format_bytes(total_b)));
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });

        // Start state saver
        let state_saver = self.spawn_state_saver();

        // Start download manager
        let (archive_tx, archive_rx) = mpsc::channel(16);
        let download_task = self.spawn_download_manager(links, archive_tx);

        // Start archive worker if enabled
        let archive_task = if self.config.archive.enabled {
            Some(self.spawn_archive_worker(archive_rx))
        } else {
            None
        };

        // Wait for shutdown or completion
        tokio::select! {
            _ = signal::ctrl_c() => {
                progress_bar.finish_with_message("Interrupted");
                info!("Shutdown signal received...");
                self.shutdown.store(true, Ordering::Relaxed);
            }
            _ = download_task => {
                let bytes = self.get_total_bytes();
                progress_bar.finish_with_message(format!("Done! {} total", Self::format_bytes(bytes)));
                info!("All downloads completed");
            }
        }

        progress_updater.abort();

        // Cleanup
        state_saver.abort();
        if let Some(task) = archive_task {
            task.abort();
        }

        self.save_state().await?;
        info!("Shutdown complete");
        Ok(())
    }

    fn extract_name(link: &str) -> String {
        // Try to extract from dn= parameter
        if let Some(start) = link.find("dn=") {
            let name_part = &link[start + 3..];
            let end = name_part.find('&').unwrap_or(name_part.len());
            if let Ok(decoded) = urlencoding::decode(&name_part[..end]) {
                return decoded.into_owned();
            }
        }
        // Fallback to hash
        if let Some(start) = link.to_lowercase().find("btih:") {
            let hash: String = link[start + 5..]
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .take(8)
                .collect();
            return format!("torrent_{}", hash);
        }
        "unknown".to_string()
    }

    fn spawn_state_saver(&self) -> tokio::task::JoinHandle<()> {
        let state = self.state.clone();
        let status_map = self.status_map.clone();
        let path = self.config.state.state_file.clone();
        let interval = self.config.state.save_interval;
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_secs(interval));
            loop {
                ticker.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Sync status_map to state
                let mut state_guard = state.write().await;
                for torrent in &mut state_guard.torrents {
                    if let Some(status) = status_map.get(&torrent.link) {
                        torrent.status = status.clone();
                    }
                }
                drop(state_guard);

                let state_guard = state.read().await;
                if let Ok(content) = serde_json::to_string_pretty(&*state_guard) {
                    let _ = fs::write(&path, content);
                }
            }
        })
    }

    fn spawn_download_manager(
        &self,
        links: Vec<String>,
        archive_tx: mpsc::Sender<(String, PathBuf)>,
    ) -> tokio::task::JoinHandle<()> {
        let config = self.config.clone();
        let status_map = self.status_map.clone();
        let shutdown = self.shutdown.clone();
        let completed_torrents = self.completed_torrents.clone();
        let completed_bytes = self.completed_bytes.clone();
        let active_downloads = self.active_downloads.clone();

        tokio::spawn(async move {
            let semaphore = Arc::new(Semaphore::new(config.download.max_concurrent));
            let mut handles = vec![];

            for link in links {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Skip completed
                if let Some(status) = status_map.get(&link) {
                    match *status {
                        TorrentStatus::Downloaded | TorrentStatus::Archived => continue,
                        _ => {}
                    }
                }

                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let config = config.clone();
                let status_map = status_map.clone();
                let shutdown = shutdown.clone();
                let archive_tx = archive_tx.clone();
                let completed_torrents = completed_torrents.clone();
                let completed_bytes = completed_bytes.clone();
                let active_downloads = active_downloads.clone();

                let handle = tokio::spawn(async move {
                    let name = Self::extract_name(&link);
                    info!("Starting download: {}", name);
                    status_map.insert(link.clone(), TorrentStatus::Downloading);
                    active_downloads.insert(link.clone(), 0);

                    match Self::download_with_aria2c(&link, &config, &shutdown, &active_downloads).await {
                        Ok((path, bytes)) => {
                            info!("✓ Completed: {} ({})", name, Self::format_bytes(bytes));
                            active_downloads.remove(&link);
                            status_map.insert(link.clone(), TorrentStatus::Downloaded);
                            completed_torrents.fetch_add(1, Ordering::Relaxed);
                            completed_bytes.fetch_add(bytes, Ordering::Relaxed);
                            let _ = archive_tx.send((name, path)).await;
                        }
                        Err(e) => {
                            let partial = active_downloads.get(&link).map(|v| *v).unwrap_or(0);
                            active_downloads.remove(&link);
                            if shutdown.load(Ordering::Relaxed) {
                                warn!("⏸ Interrupted: {} ({})", name, Self::format_bytes(partial));
                            } else {
                                error!("✗ Failed: {} - {}", name, e);
                                error!("  Reason: {}", Self::get_failure_reason(&e));
                                status_map.insert(link.clone(), TorrentStatus::Failed);
                            }
                        }
                    }
                    drop(permit);
                });

                handles.push(handle);
            }

            for handle in handles {
                let _ = handle.await;
            }
        })
    }

    async fn download_with_aria2c(
        link: &str,
        config: &Config,
        shutdown: &Arc<AtomicBool>,
        active_downloads: &Arc<DashMap<String, u64>>,
    ) -> Result<(PathBuf, u64)> {
        let download_dir = PathBuf::from(&config.download.download_dir);
        let link_key = link.to_string();

        let mut cmd = Command::new("aria2c");

        // Public trackers to help find peers
        let default_trackers = [
            "udp://tracker.opentrackr.org:1337/announce",
            "udp://open.tracker.cl:1337/announce",
            "udp://tracker.openbittorrent.com:6969/announce",
            "udp://open.stealth.si:80/announce",
            "udp://tracker.torrent.eu.org:451/announce",
            "udp://exodus.desync.com:6969/announce",
            "udp://tracker.moeking.me:6969/announce",
            "udp://explodie.org:6969/announce",
            "udp://tracker1.bt.moack.co.kr:80/announce",
            "udp://tracker.theoks.net:6969/announce",
        ];

        // Basic options
        cmd.arg(link)
            .arg("-d").arg(&download_dir)
            .arg("--seed-time=0")  // Don't seed after download
            .arg("--bt-stop-timeout=600")  // Stop if no progress for 10 min
            .arg("--summary-interval=1")  // More frequent updates
            .arg("--console-log-level=notice")
            .arg("--download-result=hide")
            .arg("--allow-overwrite=true")
            .arg("--auto-file-renaming=false")
            .arg("--bt-enable-lpd=true")  // Local peer discovery
            .arg("--enable-dht=true")
            .arg("--dht-listen-port=6881-6999")
            .arg("--enable-peer-exchange=true")
            .arg("--bt-max-peers=200")
            .arg("--bt-request-peer-speed-limit=10M")
            .arg("--max-connection-per-server=16")
            .arg("--split=16")
            .arg("--min-split-size=1M")
            .arg("--bt-tracker-connect-timeout=10")
            .arg("--bt-tracker-timeout=30")
            .arg("--human-readable=false")  // Machine-readable byte counts
            .arg(format!("--bt-tracker={}", default_trackers.join(",")));

        // Speed limits
        if let Some(ref max_dl) = config.download.max_download_speed {
            cmd.arg(format!("--max-download-limit={}", max_dl));
        }
        if let Some(ref max_ul) = config.download.max_upload_speed {
            cmd.arg(format!("--max-upload-limit={}", max_ul));
        }

        // Seed ratio
        if let Some(ratio) = config.download.seed_ratio {
            cmd.arg(format!("--seed-ratio={}", ratio));
        }

        // Additional trackers
        if let Some(ref trackers) = config.download.bt_tracker {
            let tracker_list = trackers.join(",");
            cmd.arg(format!("--bt-tracker={}", tracker_list));
        }

        // Timeout
        cmd.arg(format!("--bt-tracker-timeout={}", config.download.timeout));
        cmd.arg(format!("--timeout={}", config.download.timeout));

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn aria2c")?;

        // aria2c outputs progress to stderr, not stdout
        let stderr = child.stderr.take().unwrap();
        let mut reader = BufReader::new(stderr).lines();

        // Monitor output and parse download size
        let shutdown_clone = shutdown.clone();
        let active_clone = active_downloads.clone();
        let link_clone = link_key.clone();
        let current_bytes = Arc::new(AtomicU64::new(0));
        let bytes_clone = current_bytes.clone();
        let download_dir_clone = download_dir.clone();
        let monitor = tokio::spawn(async move {
            let mut last_scan = std::time::Instant::now();
            while let Ok(Some(line)) = reader.next_line().await {
                // Strip ANSI escape codes
                let clean_line = Self::strip_ansi(&line);

                // Parse size from aria2c output
                // With --human-readable=false, format is like: "[#abc123 1234567/9876543(12%)"
                // With human-readable, format is like: "[#abc123 1.2GiB/3.5GiB(34%)"
                if clean_line.contains("[#") {
                    let mut found = false;

                    // Try to extract downloaded size - format is "SIZE/TOTAL" or "SIZE/TOTAL(percent%)"
                    for part in clean_line.split_whitespace() {
                        if part.contains('/') && !part.starts_with('[') && !part.contains("FILE:") {
                            // Remove percent suffix if present: "123/456(50%)" -> "123/456"
                            let clean_part = if let Some(paren_pos) = part.find('(') {
                                &part[..paren_pos]
                            } else {
                                part
                            };

                            if let Some(slash_pos) = clean_part.find('/') {
                                let downloaded = &clean_part[..slash_pos];
                                // Try parsing as raw bytes first (--human-readable=false)
                                if let Ok(bytes) = downloaded.parse::<u64>() {
                                    if bytes > 0 {
                                        bytes_clone.store(bytes, Ordering::Relaxed);
                                        active_clone.insert(link_clone.clone(), bytes);
                                        found = true;
                                        break;
                                    }
                                }
                                // Fallback to human-readable format (e.g., "1.2GiB")
                                if let Some(bytes) = Self::parse_size(downloaded) {
                                    if bytes > 0 {
                                        bytes_clone.store(bytes, Ordering::Relaxed);
                                        active_clone.insert(link_clone.clone(), bytes);
                                        found = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    // If parsing failed but we're downloading, scan directory every 2 seconds
                    if !found && last_scan.elapsed() > Duration::from_secs(2) {
                        if let Ok(size) = Self::get_dir_size(&download_dir_clone) {
                            if size > 0 {
                                bytes_clone.store(size, Ordering::Relaxed);
                                active_clone.insert(link_clone.clone(), size);
                            }
                        }
                        last_scan = std::time::Instant::now();
                    }
                }
            }
        });

        // Wait for completion or shutdown
        loop {
            tokio::select! {
                status = child.wait() => {
                    monitor.abort();
                    let status = status?;
                    let final_bytes = current_bytes.load(Ordering::Relaxed);

                    // If we didn't capture bytes during download, check directory size
                    let bytes = if final_bytes == 0 {
                        Self::get_dir_size(&download_dir).unwrap_or(0)
                    } else {
                        final_bytes
                    };

                    if status.success() {
                        return Ok((download_dir, bytes));
                    } else {
                        anyhow::bail!("aria2c exited with status: {}", status);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if shutdown_clone.load(Ordering::Relaxed) {
                        let _ = child.kill().await;
                        anyhow::bail!("Download cancelled");
                    }
                }
            }
        }
    }

    fn parse_size(s: &str) -> Option<u64> {
        let s = s.trim();
        let (num_str, multiplier) = if s.ends_with("GiB") {
            (&s[..s.len()-3], 1024u64 * 1024 * 1024)
        } else if s.ends_with("MiB") {
            (&s[..s.len()-3], 1024u64 * 1024)
        } else if s.ends_with("KiB") {
            (&s[..s.len()-3], 1024u64)
        } else if s.ends_with("B") {
            (&s[..s.len()-1], 1u64)
        } else {
            return None;
        };
        num_str.parse::<f64>().ok().map(|n| (n * multiplier as f64) as u64)
    }

    /// Strip ANSI escape codes from a string
    fn strip_ansi(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip escape sequence: ESC [ ... letter
                if chars.peek() == Some(&'[') {
                    chars.next(); // consume '['
                    // Consume until we hit a letter (the terminator)
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                result.push(c);
            }
        }
        result
    }

    fn get_dir_size(path: &Path) -> Result<u64> {
        let mut total = 0u64;
        if path.is_dir() {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    total += Self::get_dir_size(&path)?;
                } else {
                    total += entry.metadata()?.len();
                }
            }
        } else if path.is_file() {
            total = fs::metadata(path)?.len();
        }
        Ok(total)
    }

    fn get_failure_reason(err: &anyhow::Error) -> &'static str {
        let msg = err.to_string().to_lowercase();
        if msg.contains("timeout") || msg.contains("timed out") {
            "Connection timed out - no peers found or slow network"
        } else if msg.contains("no peer") || msg.contains("0 seeder") {
            "No seeders available for this torrent"
        } else if msg.contains("metadata") {
            "Could not fetch torrent metadata - torrent may be dead"
        } else if msg.contains("cancelled") || msg.contains("interrupted") {
            "Download was cancelled by user"
        } else if msg.contains("exit") && msg.contains("7") {
            "aria2c error: No peers/seeders found after timeout"
        } else if msg.contains("exit") && msg.contains("3") {
            "aria2c error: Resource not found"
        } else if msg.contains("exit") && msg.contains("24") {
            "aria2c error: HTTP authorization failed"
        } else if msg.contains("disk") || msg.contains("space") {
            "Disk full or write error"
        } else if msg.contains("permission") {
            "Permission denied - check folder access"
        } else {
            "Unknown error - check if torrent is still active"
        }
    }

    fn spawn_archive_worker(
        &self,
        mut rx: mpsc::Receiver<(String, PathBuf)>,
    ) -> tokio::task::JoinHandle<()> {
        let config = self.config.clone();
        let status_map = self.status_map.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            while let Some((name, path)) = rx.recv().await {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                info!("Archiving: {}", name);

                // Find the link for this name
                let link = status_map
                    .iter()
                    .find(|entry| Self::extract_name(entry.key()) == name)
                    .map(|entry| entry.key().clone());

                if let Some(link) = link {
                    status_map.insert(link.clone(), TorrentStatus::Archiving);

                    match Self::archive_with_7z(&name, &path, &config.archive).await {
                        Ok(_) => {
                            info!("Archived: {}", name);
                            status_map.insert(link, TorrentStatus::Archived);
                        }
                        Err(e) => {
                            error!("Archive failed {}: {}", name, e);
                            status_map.insert(link, TorrentStatus::Failed);
                        }
                    }
                }
            }
        })
    }

    async fn archive_with_7z(name: &str, source_dir: &Path, config: &ArchiveConfig) -> Result<()> {
        let archive_path = PathBuf::from(&config.archive_dir).join(format!("{}.7z", name));

        // Find the actual downloaded files/folder
        let source = source_dir.join(name);
        let source_path = if source.exists() {
            source
        } else {
            // Maybe it's directly in download_dir
            source_dir.to_path_buf()
        };

        let mut cmd = std::process::Command::new("7z");
        cmd.arg("a")
            .arg(format!("-mx={}", config.compression_level))
            .arg(&archive_path)
            .arg(&source_path);

        let output = cmd.output().context("7z not found")?;

        if !output.status.success() {
            anyhow::bail!("7z failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        // Delete source if configured
        if config.delete_after_archive && source_path.exists() {
            if source_path.is_dir() {
                fs::remove_dir_all(&source_path)?;
            } else {
                fs::remove_file(&source_path)?;
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let config_content = fs::read_to_string("config.toml")
        .context("Failed to read config.toml")?;
    let config: Config = toml::from_str(&config_content)?;

    info!("Starting torrent scraper with {} concurrent downloads", config.download.max_concurrent);

    let app = App::new(config)?;
    app.run().await?;

    Ok(())
}
