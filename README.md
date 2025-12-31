# Torrent Scraping Tool

## Setup
Make sure you have `rust`, `cargo`, `rustc` and `rustup` installed. On Linux Debian Systems, this usually works with `apt install`:
```bash
sudo apt update -y && sudo apt install rust-all rustup && rustup default stable
```
Other Systems wherent tested yet. If you run into any issues, please open an Github Issue and tell us how you fixed it (if you did). 

**If you still run into any errors** try updating rust:
```bash
cargo update && rustup update
```

## Extract
To **extract** `magnet links` from any site you want, use the `extract` tool (in the `extract` crate). The following args are supported:

> **Note:** this project requires **rustc 1.82.0 or newer**. If not properly configured, building will fail.

**Build the Extractor first**
```bash
cd extract/
cargo build --release
```

**Run from the crate**:
```bash
cargo run --bin extract -- --domain example.com
```

**Run with Custom parameters**
```bash
cargo run --bin extract -- \
  --domain example.com \
  --max-links 500 \
  --concurrent 20 \
  --max-depth 5 \
  --timeout 60 \
  --output my_torrents.json
```

> **Important** Copy the newly created `torrents.json` file from the `extract/` crate into the ``scrape/` crate if you want to use it for scraping.

## Scraper

**Build the Scraper first**:
````bash
cargo build --release
```

To run the scraper, edit the `config.toml` file to your preferences and build/run the `scrape` crate:

**Run From the crate**:
```bash
cd scrape/
cargo build --release
cargo run
```


## Features

### Extractor:
- Concurrent crawling with configurable connection limit
- Depth-limited crawling to avoid going too deep
- Domain-scoped - only follows links within the specified domain
- Duplicate prevention - tracks visited URLs and unique magnets
- Regex-based magnet extraction from page source and HTML
- Progress logging via tracing
- Configurable timeout for requests
- Auto-saves to torrents.json in your specified format

### Scraper
~~Add Features here~~
