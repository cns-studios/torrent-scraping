# Torrent Scraping Tool

## Extract
To **extract** `magnet links` from any site you want, use the `extract` tool (in the `extract` crate). The following args are supported:
**Build the Extractor first**
```bash
cd extract/
cargo build --release
```

Run from the crate:
```bash
cargo run --bin extract -- --domain example.com
```

### Custom parameters
```bash
cargo run --bin extract -- \
  --domain example.com \
  --max-links 500 \
  --concurrent 20 \
  --max-depth 5 \
  --timeout 60 \
  --output my_torrents.json
```
## Scraper

To run the scraper, edit the `config.toml` file to your preferences and build/run the `scrape` crate:

From the crate:
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
