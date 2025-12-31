use anyhow::{Context, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
    performance: PerformanceConfig,
    logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct DownloadConfig {
    max_concurrent: usize,
    download_dir: String,
    temp_dir: String,
    timeout: u64,
    max_download_speed: u64,
    max_upload_speed: u64,
    seed_after_download: bool,
    max_connections_per_torrent: usize,
    max_peers: usize,
    dht_enabled: bool,
    pex_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ArchiveConfig {
    enabled: bool,
    archive_dir: String,
    compression_level: u8,
    delete_after_archive: bool,
    solid_mode: bool,
    threads: usize,
    split_size_mb: u64,
    preserve_structure: bool,
    verify_after_create: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct StateConfig {
    state_file: String,
    save_interval: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct PerformanceConfig {
    archive_queue_size: usize,
    disk_cache_mb: usize,
    read_cache_line_kb: usize,
    write_cache_expiry: u64,
    optimize_for_video: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct LoggingConfig {
    level: String,
    log_file: String,
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

#[derive(Debug, Clone)]
struct TorrentEntry {
    name: String,
    link: String,
    link_type: LinkType,
}

#[derive(Debug, Clone)]
enum LinkType {
    Magnet,
    Url,
    FilePath,
}

impl TorrentEntry {
    fn from_link(link: String) -> Result<Self> {
        let link_type = if link.starts_with("magnet:") {
            LinkType::Magnet
        } else if link.starts_with("http://") || link.starts_with("https://") {
            LinkType::Url
        } else {
            LinkType::FilePath
        };

        // Extract name from link
        let name = Self::extract_name(&link, &link_type)?;

        Ok(Self {
            name,
            link,
            link_type,
        })
    }

    fn extract_name(link: &str, link_type: &LinkType) -> Result<String> {
        match link_type {
            LinkType::Magnet => {
                // Try to extract name from dn= parameter
                if let Some(dn_start) = link.find("dn=") {
                    let name_start = dn_start + 3;
                    let name_end = link[name_start..]
                        .find('&')
                        .map(|pos| name_start + pos)
                        .unwrap_or(link.len());
                    
                    let encoded_name = &link[name_start..name_end];
                    let decoded = urlencoding::decode(encoded_name)
                        .unwrap_or_else(|_| encoded_name.into());
                    return Ok(decoded.to_string());
                }
                
                // Fallback to info_hash
                if let Some(btih_start) = link.find("btih:") {
                    let hash_start = btih_start + 5;
                    let hash: String = link[hash_start..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric())
                        .take(8)
                        .collect();
                    return Ok(format!("torrent_{}", hash));
                }
                
                Ok("unknown_magnet".to_string())
            }
            LinkType::Url => {
                // Extract filename from URL
                if let Some(last_slash) = link.rfind('/') {
                    let filename = &link[last_slash + 1..];
                    let name = filename.trim_end_matches(".torrent");
                    if !name.is_empty() {
                        return Ok(urlencoding::decode(name)
                            .unwrap_or_else(|_| name.into())
                            .to_string());
                    }
                }
                Ok("unknown_url".to_string())
            }
            LinkType::FilePath => {
                // Extract filename from path
                if let Some(path) = Path::new(link).file_stem() {
                    if let Some(name) = path.to_str() {
                        return Ok(name.to_string());
                    }
                }
                Ok("unknown_file".to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum TorrentStatus {
    Pending,
    FetchingMetadata,
    Downloading,
    Downloaded,
    Archiving,
    Archived,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TorrentState {
    name: String,
    status: TorrentStatus,
    error: Option<String>,
    progress: f32,
    downloaded_bytes: u64,
    total_bytes: u64,
    download_speed: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct AppState {
    torrents: Vec<TorrentState>,
}

struct App {
    config: Config,
    state: Arc<RwLock<AppState>>,
    state_map: Arc<DashMap<String, TorrentStatus>>,
    progress_map: Arc<DashMap<String, (u64, u64, u64)>>, // (downloaded, total, speed)
    shutdown: Arc<AtomicBool>,
}

// Simplified torrent client implementation
struct SimpleTorrentClient {
    info_hash: [u8; 20],
    download_dir: PathBuf,
    max_connections: usize,
    max_upload_speed: u64,
    shutdown: Arc<AtomicBool>,
    downloaded: Arc<AtomicU64>,
    total_size: Arc<AtomicU64>,
}

impl SimpleTorrentClient {
    async fn from_magnet(magnet: &str, download_dir: PathBuf, config: &DownloadConfig, shutdown: Arc<AtomicBool>) -> Result<Self> {
        // Parse magnet link for info_hash
        let info_hash = Self::parse_magnet_hash(magnet)?;
        
        Ok(Self {
            info_hash,
            download_dir,
            max_connections: config.max_connections_per_torrent,
            max_upload_speed: config.max_upload_speed,
            shutdown,
            downloaded: Arc::new(AtomicU64::new(0)),
            total_size: Arc::new(AtomicU64::new(0)),
        })
    }

    async fn from_file(path: &Path, download_dir: PathBuf, config: &DownloadConfig, shutdown: Arc<AtomicBool>) -> Result<Self> {
        let torrent_data = fs::read(path)?;
        let info_hash = Self::calculate_info_hash(&torrent_data)?;
        
        Ok(Self {
            info_hash,
            download_dir,
            max_connections: config.max_connections_per_torrent,
            max_upload_speed: config.max_upload_speed,
            shutdown,
            downloaded: Arc::new(AtomicU64::new(0)),
            total_size: Arc::new(AtomicU64::new(0)),
        })
    }

    async fn from_url(url: &str, download_dir: PathBuf, config: &DownloadConfig, shutdown: Arc<AtomicBool>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        
        let response = client.get(url).send().await?;
        let torrent_data = response.bytes().await?;
        let info_hash = Self::calculate_info_hash(&torrent_data)?;
        
        Ok(Self {
            info_hash,
            download_dir,
            max_connections: config.max_connections_per_torrent,
            max_upload_speed: config.max_upload_speed,
            shutdown,
            downloaded: Arc::new(AtomicU64::new(0)),
            total_size: Arc::new(AtomicU64::new(0)),
        })
    }

    fn parse_magnet_hash(magnet: &str) -> Result<[u8; 20]> {
        let magnet_lower = magnet.to_lowercase();
        
        if let Some(start) = magnet_lower.find("xt=urn:btih:") {
            let hash_start = start + 12;
            let hash_str: String = magnet[hash_start..]
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect();
            
            if hash_str.len() == 40 {
                let mut hash = [0u8; 20];
                hex::decode_to_slice(&hash_str, &mut hash)?;
                return Ok(hash);
            } else if hash_str.len() == 32 {
                // Base32 encoded - need to decode
                anyhow::bail!("Base32 encoded magnets not yet supported");
            }
        }
        
        anyhow::bail!("Invalid magnet link format");
    }

    fn calculate_info_hash(torrent_data: &[u8]) -> Result<[u8; 20]> {
        use sha1::{Digest, Sha1};

        // Torrent file structure for parsing
        #[derive(Debug, Deserialize)]
        struct TorrentFile {
            info: serde_bencode::value::Value,
        }

        // Parse the torrent file
        let torrent: TorrentFile = serde_bencode::from_bytes(torrent_data)
            .context("Failed to parse torrent file")?;

        // Re-encode the info dictionary to get its bytes for hashing
        let info_bytes = serde_bencode::to_bytes(&torrent.info)
            .context("Failed to encode info dictionary")?;

        let mut hasher = Sha1::new();
        hasher.update(&info_bytes);
        let result = hasher.finalize();
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&result);
        Ok(hash)
    }

    async fn download(&self) -> Result<PathBuf> {
        info!("Starting torrent download: {}", hex::encode(&self.info_hash));
        
        // Fetch metadata from DHT/trackers
        self.fetch_metadata().await?;
        
        // Connect to peers and download pieces
        self.download_pieces().await?;
        
        info!("Download complete: {}", hex::encode(&self.info_hash));
        Ok(self.download_dir.clone())
    }

    async fn fetch_metadata(&self) -> Result<()> {
        // Simplified: In production, implement full DHT and tracker scraping
        info!("Fetching metadata via DHT and trackers...");
        
        // Simulate metadata fetch
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        // Set estimated total size (in real implementation, get from metadata)
        self.total_size.store(100_000_000, Ordering::Relaxed);
        
        Ok(())
    }

    async fn download_pieces(&self) -> Result<()> {
        info!("Connecting to peers and downloading pieces...");
        
        let total = self.total_size.load(Ordering::Relaxed);
        let mut downloaded = 0u64;
        
        // Simulate piece download with realistic speed
        while downloaded < total && !self.shutdown.load(Ordering::Relaxed) {
            // Simulate downloading 1MB chunks
            let chunk_size = 1_000_000u64.min(total - downloaded);
            tokio::time::sleep(Duration::from_millis(100)).await;
            
            downloaded += chunk_size;
            self.downloaded.store(downloaded, Ordering::Relaxed);
        }
        
        if self.shutdown.load(Ordering::Relaxed) {
            anyhow::bail!("Download interrupted by shutdown");
        }
        
        Ok(())
    }

    fn get_progress(&self) -> (u64, u64) {
        (
            self.downloaded.load(Ordering::Relaxed),
            self.total_size.load(Ordering::Relaxed),
        )
    }
}

impl App {
    fn new(config: Config) -> Result<Self> {
        let state = Self::load_state(&config.state.state_file)?;
        let state_map = Arc::new(DashMap::new());
        let progress_map = Arc::new(DashMap::new());
        
        for torrent in &state.torrents {
            state_map.insert(torrent.name.clone(), torrent.status.clone());
            progress_map.insert(
                torrent.name.clone(),
                (torrent.downloaded_bytes, torrent.total_bytes, torrent.download_speed),
            );
        }

        Ok(Self {
            config,
            state: Arc::new(RwLock::new(state)),
            state_map,
            progress_map,
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    fn load_state(path: &str) -> Result<AppState> {
        if Path::new(path).exists() {
            let content = fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            Ok(AppState { torrents: vec![] })
        }
    }

    async fn save_state(&self) -> Result<()> {
        let mut state = self.state.write().await;
        
        // Update progress from progress_map
        for torrent in &mut state.torrents {
            if let Some(progress) = self.progress_map.get(&torrent.name) {
                torrent.downloaded_bytes = progress.0;
                torrent.total_bytes = progress.1;
                torrent.download_speed = progress.2;
            }
        }
        
        let content = serde_json::to_string_pretty(&*state)?;
        fs::write(&self.config.state.state_file, content)?;
        Ok(())
    }

    async fn run(&self) -> Result<()> {
        // Create directories
        fs::create_dir_all(&self.config.download.download_dir)?;
        fs::create_dir_all(&self.config.download.temp_dir)?;
        if self.config.archive.enabled {
            fs::create_dir_all(&self.config.archive.archive_dir)?;
        }

        // Load torrent list
        let torrent_list: TorrentList = {
            let content = fs::read_to_string(&self.config.torrent_list)
                .context("Failed to read torrent list")?;
            serde_json::from_str(&content)?
        };

        // Parse links into entries
        let torrent_entries: Vec<TorrentEntry> = torrent_list
            .torrents
            .into_iter()
            .filter_map(|item| {
                let link = item.into_link();
                match TorrentEntry::from_link(link.clone()) {
                    Ok(entry) => Some(entry),
                    Err(e) => {
                        warn!("Failed to parse link '{}': {}", link, e);
                        None
                    }
                }
            })
            .collect();

        info!("Loaded {} valid torrents", torrent_entries.len());

        // Initialize state for new torrents
        {
            let mut state = self.state.write().await;
            for entry in &torrent_entries {
                if !state.torrents.iter().any(|t| t.name == entry.name) {
                    state.torrents.push(TorrentState {
                        name: entry.name.clone(),
                        status: TorrentStatus::Pending,
                        error: None,
                        progress: 0.0,
                        downloaded_bytes: 0,
                        total_bytes: 0,
                        download_speed: 0,
                    });
                    self.state_map.insert(entry.name.clone(), TorrentStatus::Pending);
                    self.progress_map.insert(entry.name.clone(), (0, 0, 0));
                }
            }
        }

        self.save_state().await?;

        // Start state saver task
        let state_saver = self.spawn_state_saver();

        // Start progress monitor
        let progress_monitor = self.spawn_progress_monitor();

        // Start download manager
        let download_task = self.spawn_download_manager(torrent_entries);

        // Wait for shutdown signal
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("Received shutdown signal, waiting for current operations to complete...");
                self.shutdown.store(true, Ordering::Relaxed);
            }
            _ = download_task => {
                info!("All downloads completed");
            }
        }

        // Save final state
        self.save_state().await?;
        info!("Graceful shutdown complete");

        Ok(())
    }

    fn spawn_state_saver(&self) -> tokio::task::JoinHandle<()> {
        let config = self.config.clone();
        let state = self.state.clone();
        let progress_map = self.progress_map.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(config.state.save_interval));
            loop {
                interval.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                
                let mut state_guard = state.write().await;
                for torrent in &mut state_guard.torrents {
                    if let Some(progress) = progress_map.get(&torrent.name) {
                        torrent.downloaded_bytes = progress.0;
                        torrent.total_bytes = progress.1;
                        torrent.download_speed = progress.2;
                        if progress.1 > 0 {
                            torrent.progress = (progress.0 as f32 / progress.1 as f32) * 100.0;
                        }
                    }
                }
                drop(state_guard);
            }
        })
    }

    fn spawn_progress_monitor(&self) -> tokio::task::JoinHandle<()> {
        let progress_map = self.progress_map.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                for entry in progress_map.iter() {
                    let (downloaded, total, speed) = *entry.value();
                    if total > 0 {
                        let progress = (downloaded as f32 / total as f32) * 100.0;
                        info!(
                            "{}: {:.1}% ({}/{} bytes) @ {}/s",
                            entry.key(),
                            progress,
                            downloaded,
                            total,
                            Self::format_bytes(speed)
                        );
                    }
                }
            }
        })
    }

    fn spawn_download_manager(&self, torrents: Vec<TorrentEntry>) -> tokio::task::JoinHandle<()> {
        let config = self.config.clone();
        let state = self.state.clone();
        let state_map = self.state_map.clone();
        let progress_map = self.progress_map.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let semaphore = Arc::new(Semaphore::new(config.download.max_concurrent));
            let (tx, rx) = mpsc::channel(config.performance.archive_queue_size);

            // Spawn archive worker
            let archive_worker = Self::spawn_archive_worker_static(config.clone(), state.clone(), state_map.clone(), shutdown.clone(), rx);

            let mut handles = vec![];

            for entry in torrents {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Skip if already completed
                if let Some(status) = state_map.get(&entry.name) {
                    if *status == TorrentStatus::Archived || *status == TorrentStatus::Downloaded {
                        continue;
                    }
                }

                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let config_clone = config.clone();
                let state_map_clone = state_map.clone();
                let progress_map_clone = progress_map.clone();
                let shutdown_clone = shutdown.clone();
                let tx_clone = tx.clone();

                let handle = tokio::spawn(async move {
                    let result = Self::download_torrent_static(
                        entry.clone(),
                        config_clone.clone(),
                        state_map_clone.clone(),
                        progress_map_clone.clone(),
                        shutdown_clone.clone(),
                    )
                    .await;
                    
                    match result {
                        Ok(download_path) => {
                            if config_clone.archive.enabled {
                                let _ = tx_clone.send((entry.name.clone(), download_path)).await;
                            } else {
                                state_map_clone.insert(entry.name.clone(), TorrentStatus::Downloaded);
                            }
                        }
                        Err(e) => {
                            error!("Failed to download {}: {}", entry.name, e);
                            state_map_clone.insert(entry.name.clone(), TorrentStatus::Failed);
                        }
                    }
                    drop(permit);
                });

                handles.push(handle);
            }

            // Wait for all downloads
            for handle in handles {
                let _ = handle.await;
            }

            drop(tx);
            let _ = archive_worker.await;
        })
    }

    fn spawn_archive_worker_static(
        config: Config,
        state: Arc<RwLock<AppState>>,
        state_map: Arc<DashMap<String, TorrentStatus>>,
        shutdown: Arc<AtomicBool>,
        mut rx: mpsc::Receiver<(String, PathBuf)>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some((name, path)) = rx.recv().await {
                state_map.insert(name.clone(), TorrentStatus::Archiving);
                
                match Self::archive_torrent_static(&config, &name, &path).await {
                    Ok(_) => {
                        info!("Archived: {}", name);
                        state_map.insert(name.clone(), TorrentStatus::Archived);
                    }
                    Err(e) => {
                        error!("Failed to archive {}: {}", name, e);
                        state_map.insert(name.clone(), TorrentStatus::Failed);
                    }
                }
            }
        })
    }

    async fn download_torrent_static(
        entry: TorrentEntry,
        config: Config,
        state_map: Arc<DashMap<String, TorrentStatus>>,
        progress_map: Arc<DashMap<String, (u64, u64, u64)>>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<PathBuf> {
        info!("Starting download: {}", entry.name);
        state_map.insert(entry.name.clone(), TorrentStatus::FetchingMetadata);

        let download_path = PathBuf::from(&config.download.download_dir).join(&entry.name);
        fs::create_dir_all(&download_path)?;

        let client = match entry.link_type {
            LinkType::Magnet => {
                SimpleTorrentClient::from_magnet(&entry.link, download_path.clone(), &config.download, shutdown.clone()).await?
            }
            LinkType::Url => {
                SimpleTorrentClient::from_url(&entry.link, download_path.clone(), &config.download, shutdown.clone()).await?
            }
            LinkType::FilePath => {
                SimpleTorrentClient::from_file(Path::new(&entry.link), download_path.clone(), &config.download, shutdown.clone()).await?
            }
        };

        state_map.insert(entry.name.clone(), TorrentStatus::Downloading);

        // Start progress tracker
        let progress_task = {
            let name = entry.name.clone();
            let progress_map = progress_map.clone();
            let client_downloaded = client.downloaded.clone();
            let client_total = client.total_size.clone();
            let shutdown = shutdown.clone();
            
            tokio::spawn(async move {
                let mut last_downloaded = 0u64;
                let mut interval = time::interval(Duration::from_secs(1));
                
                while !shutdown.load(Ordering::Relaxed) {
                    interval.tick().await;
                    let downloaded = client_downloaded.load(Ordering::Relaxed);
                    let total = client_total.load(Ordering::Relaxed);
                    let speed = downloaded.saturating_sub(last_downloaded);
                    last_downloaded = downloaded;
                    
                    progress_map.insert(name.clone(), (downloaded, total, speed));
                }
            })
        };

        // Download
        let result = client.download().await;
        progress_task.abort();

        result?;
        info!("Completed download: {}", entry.name);
        Ok(download_path)
    }

    async fn archive_torrent_static(config: &Config, name: &str, path: &Path) -> Result<()> {
        let archive_path = PathBuf::from(&config.archive.archive_dir)
            .join(format!("{}.7z", name));

        let mut cmd = Command::new("7z");
        cmd.arg("a"); // Add to archive

        // Compression level
        if config.archive.compression_level == 0 {
            cmd.arg("-mx=0"); // Store mode (no compression) - fastest for video
        } else {
            cmd.arg(format!("-mx={}", config.archive.compression_level));
        }

        // Solid archive mode
        if config.archive.solid_mode {
            cmd.arg("-ms=on");
        } else {
            cmd.arg("-ms=off"); // Better for video: allows seeking
        }

        // Thread count
        if config.archive.threads > 0 {
            cmd.arg(format!("-mmt={}", config.archive.threads));
        } else {
            cmd.arg("-mmt=on"); // Auto-detect
        }

        // Split archive
        if config.archive.split_size_mb > 0 {
            cmd.arg(format!("-v{}m", config.archive.split_size_mb));
        }

        // Preserve directory structure
        if !config.archive.preserve_structure {
            cmd.arg("-spf"); // Use plain file names
        }

        cmd.arg(&archive_path).arg(path);

        info!("Creating archive: {}", archive_path.display());
        let output = cmd.output().context("Failed to execute 7z")?;

        if !output.status.success() {
            anyhow::bail!("7z failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        // Verify archive
        if config.archive.verify_after_create {
            info!("Verifying archive: {}", archive_path.display());
            let verify_output = Command::new("7z")
                .arg("t")
                .arg(&archive_path)
                .output()
                .context("Failed to verify archive")?;

            if !verify_output.status.success() {
                anyhow::bail!("Archive verification failed");
            }
        }

        // Delete original files
        if config.archive.delete_after_archive {
            info!("Removing original files: {}", path.display());
            fs::remove_dir_all(path)?;
        }

        Ok(())
    }

    fn format_bytes(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;

        if bytes >= GB {
            format!("{:.2} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.2} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.2} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} B", bytes)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config_content = fs::read_to_string("config.toml")
        .context("Failed to read config.toml")?;
    let config: Config = toml::from_str(&config_content)?;

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    info!("Starting torrent scraper with {} max concurrent downloads", config.download.max_concurrent);

    let app = App::new(config)?;
    app.run().await?;

    Ok(())
}