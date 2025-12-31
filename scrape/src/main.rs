use anyhow::{Context, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
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
        })
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

        info!("Loaded {} torrents", links.len());

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
                info!("Shutdown signal received...");
                self.shutdown.store(true, Ordering::Relaxed);
            }
            _ = download_task => {
                info!("All downloads completed");
            }
        }

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

                let handle = tokio::spawn(async move {
                    let name = Self::extract_name(&link);
                    info!("Starting download: {}", name);
                    status_map.insert(link.clone(), TorrentStatus::Downloading);

                    match Self::download_with_aria2c(&link, &config, &shutdown).await {
                        Ok(path) => {
                            info!("Completed: {}", name);
                            status_map.insert(link.clone(), TorrentStatus::Downloaded);
                            let _ = archive_tx.send((name, path)).await;
                        }
                        Err(e) => {
                            if shutdown.load(Ordering::Relaxed) {
                                warn!("Download interrupted: {}", name);
                            } else {
                                error!("Failed {}: {}", name, e);
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
    ) -> Result<PathBuf> {
        let download_dir = PathBuf::from(&config.download.download_dir);

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
            .arg("--summary-interval=5")
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

        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout).lines();

        // Monitor output
        let shutdown_clone = shutdown.clone();
        let monitor = tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                if line.contains("[#") || line.contains("Download complete") {
                    info!("{}", line);
                }
            }
        });

        // Wait for completion or shutdown
        loop {
            tokio::select! {
                status = child.wait() => {
                    monitor.abort();
                    let status = status?;
                    if status.success() {
                        return Ok(download_dir);
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
