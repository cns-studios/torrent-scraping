# Torrent Scraping Tool

A cross-platform toolkit for extracting magnet links from websites and downloading torrents.

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

> **Note:** Visual Studio Build Tools are **not required** - this project uses `rustls` for TLS, avoiding native library dependencies.

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

## Scraper

Downloads torrents from magnet links and optionally archives them with 7-Zip.

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

Edit `config.toml` to customize behavior. Key settings:

```toml
# Path to torrent list (supports extract's output format)
torrent_list = "torrents.json"

[download]
max_concurrent = 4          # Simultaneous downloads
download_dir = "./downloads"
timeout = 300               # Connection timeout (seconds)

[archive]
enabled = true              # Auto-archive completed downloads
archive_dir = "./archives"
compression_level = 0       # 0=store (best for video), 9=ultra
delete_after_archive = true
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
- Cross-platform (Windows, Linux, macOS) - pure Rust TLS
- Concurrent downloads with configurable limits
- Magnet link and .torrent file support
- Automatic 7-Zip archiving (requires 7z in PATH)
- Configurable compression (0=store for video, 1-9 for compression)
- State persistence - resume interrupted sessions
- Progress tracking and logging
- Graceful shutdown on Ctrl+C
- Flexible input format - accepts both simple and detailed JSON
- Split archive support for large files
- Archive verification after creation

---

## License

See [LICENSE](LICENSE) for details.
