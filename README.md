# Torrent Scraping Tool

A cross-platform toolkit for extracting magnet links from websites and downloading torrents.
**Be sure to enable a VPN Service like `Cloudflare 1.1.1.1` before connecting to any torrents**

## Setup

### Prerequisites
- Rust 1.82.0 or newer
- Cargo, rustc, and rustup

### Linux (Debian/Ubuntu)
```bash
sudo apt update -y && sudo apt install rustup -y
rustup default stable
```

### Windows
```powershell
winget install Rustlang.Rustup
rustup default stable
```

### macOS
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
```

### Troubleshooting
If you run into build errors, try updating:
```bash
rustup update && cargo update
```

---

## Extract

Extracts magnet links from websites. Supports both HTML scraping and JSON API parsing (e.g., TPB's apibay.org).

### Build
```bash
cargo build --release -p extract
```

### Usage

**Basic usage:**
```bash
./target/release/extract --domain example.com
```

**With custom parameters:**
```bash
./target/release/extract \
  --domain example.com \
  --max-links 500 \
  --concurrent 20 \
  --max-depth 5 \
  --timeout 60 \
  --output my_torrents.json
```

**For API-based sites (like TPB):**
```bash
# TPB uses JavaScript - hit the API directly
./target/release/extract --domain "https://apibay.org/q.php?q=ubuntu&cat=200" --max-links 50
```

### Command Line Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--domain` | `-d` | required | Domain or URL to scrape |
| `--max-links` | `-m` | 100 | Maximum magnet links to collect |
| `--concurrent` | `-c` | 10 | Number of concurrent connections |
| `--max-depth` | `-D` | 3 | Maximum crawl depth (0 = starting page only) |
| `--timeout` | `-t` | 30 | Request timeout in seconds |
| `--output` | `-o` | torrents.json | Output file path |

### Output Format
```json
{
  "torrents": [
    {
      "name": "Example Torrent",
      "magnet": "magnet:?xt=urn:btih:...",
      "size": 1234567890,
      "seeders": 100,
      "leechers": 50
    }
  ]
}
```

---

# Scrape

Downloads torrents from magnet links using aria2c and optionally archives them with 7-Zip.

### Prerequisites

**aria2c** is required for downloading. Install it:
```bash
# Windows
winget install aria2

# Linux (Debian/Ubuntu)
sudo apt install aria2

# macOS
brew install aria2
```

**7-Zip** (optional, for archiving):
```bash
# Windows
winget install 7zip

# Linux
sudo apt install p7zip-full

# macOS
brew install p7zip
```

### Build
```bash
cargo build --release -p scrape
```

### Usage
1. Copy `torrents.json` from extract output to the `scrape/` directory (or configure path in `config.toml`)
2. Edit `scrape/config.toml` to your preferences
3. Run the scraper:

```bash
cd scrape/
cargo run --release
```

### Configuration

Edit `config.toml` to customize behavior:

```toml
torrent_list = "torrents.json"

[download]
max_concurrent = 4          # Simultaneous downloads
download_dir = "./downloads"
timeout = 300               # Connection timeout (seconds)
# max_download_speed = "10M"  # Optional speed limit
# max_upload_speed = "100K"   # Optional upload limit
# seed_ratio = 0.0            # Don't seed after download

[archive]
enabled = false             # Auto-archive completed downloads
archive_dir = "./archives"
compression_level = 0       # 0=store (best for video), 9=ultra
delete_after_archive = false

[state]
state_file = "state.json"   # Resume interrupted sessions
save_interval = 30
```

### Input Format
Accepts both formats:

**Simple (array of magnet strings):**
```json
{ "torrents": ["magnet:?xt=urn:btih:...", "magnet:?xt=urn:btih:..."] }
```

**Detailed (extract output):**
```json
{ "torrents": [{ "magnet": "magnet:?xt=urn:btih:...", "name": "..." }] }
```

---

## Features

### Extractor
- Cross-platform (Windows, Linux, macOS) - no native SSL dependencies
- Concurrent crawling with configurable connection limits
- Depth-limited crawling to control scope
- Domain-scoped - only follows links within the specified domain
- Duplicate prevention - tracks visited URLs and unique magnets
- JSON API parsing - extracts info_hash from API responses (TPB, etc.)
- Regex-based magnet extraction from HTML source
- Rich metadata extraction (name, size, seeders, leechers)
- Progress logging via tracing
- Configurable timeout for requests

### Scraper
- Cross-platform (Windows, Linux, macOS)
- Uses aria2c for fast, reliable BitTorrent downloads
- Concurrent downloads with configurable limits
- DHT, PEX, and local peer discovery enabled
- No seeding by default (configurable)
- Speed limiting (upload/download)
- Automatic 7-Zip archiving (optional, requires 7z)
- State persistence - resume interrupted sessions
- Progress tracking and logging
- Graceful shutdown on Ctrl+C
- Flexible input format - accepts both simple and detailed JSON