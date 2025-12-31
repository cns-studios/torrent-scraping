use anyhow::{Context, Result};
use clap::Parser;
use dashmap::DashSet;
use futures::stream::{self, StreamExt};
use regex::Regex;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};
use url::Url;

#[derive(Parser, Debug)]
#[command(author, version, about = "Extract magnet links from a domain", long_about = None)]
struct Args {
    /// The domain to scrape (e.g., example.com)
    #[arg(short, long)]
    domain: String,

    /// Maximum number of magnet links to collect before stopping
    #[arg(short, long, default_value_t = 100)]
    max_links: usize,

    /// Number of concurrent connections
    #[arg(short, long, default_value_t = 10)]
    concurrent: usize,

    /// Maximum depth to crawl (0 = only starting page)
    #[arg(short = 'D', long, default_value_t = 3)]
    max_depth: usize,

    /// Request timeout in seconds
    #[arg(short, long, default_value_t = 30)]
    timeout: u64,

    /// Output file path
    #[arg(short, long, default_value = "torrents.json")]
    output: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct TorrentData {
    torrents: Vec<String>,
}

struct Scraper {
    client: Client,
    domain: String,
    visited: Arc<DashSet<String>>,
    magnets: Arc<DashSet<String>>,
    magnet_regex: Regex,
    semaphore: Arc<Semaphore>,
    max_links: usize,
    max_depth: usize,
}

impl Scraper {
    fn new(args: &Args) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(args.timeout))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .build()?;

        let magnet_regex = Regex::new(r#"magnet:\?[^\s<>"']+|magnet:\?[^\s<>'"]+"#).unwrap();

        Ok(Self {
            client,
            domain: args.domain.clone(),
            visited: Arc::new(DashSet::new()),
            magnets: Arc::new(DashSet::new()),
            magnet_regex,
            semaphore: Arc::new(Semaphore::new(args.concurrent)),
            max_links: args.max_links,
            max_depth: args.max_depth,
        })
    }

    fn normalize_url(&self, base: &str, href: &str) -> Option<String> {
        let base_url = Url::parse(base).ok()?;
        let resolved = base_url.join(href).ok()?;

        // Only return URLs from the same domain
        if let Some(host) = resolved.host_str() {
            if host.contains(&self.domain) {
                return Some(resolved.to_string());
            }
        }
        None
    }

    fn extract_magnets(&self, html: &str) {
        for cap in self.magnet_regex.find_iter(html) {
            let magnet = cap.as_str().to_string();
            if self.magnets.insert(magnet.clone()) {
                info!("Found magnet: {}", &magnet[..60.min(magnet.len())]);
            }
        }
    }

    fn extract_links(&self, html: &str, base_url: &str) -> Vec<String> {
        let document = Html::parse_document(html);
        let selector = Selector::parse("a[href]").unwrap();
        let mut links = Vec::new();

        for element in document.select(&selector) {
            if let Some(href) = element.value().attr("href") {
                if let Some(normalized) = self.normalize_url(base_url, href) {
                    links.push(normalized);
                }
            }
        }

        links
    }

    async fn fetch_page(&self, url: &str) -> Result<String> {
        let _permit = self.semaphore.acquire().await?;
        
        let response = self.client.get(url).send().await?;
        let status = response.status();
        
        if !status.is_success() {
            anyhow::bail!("HTTP {}: {}", status, url);
        }

        let text = response.text().await?;
        Ok(text)
    }

    async fn crawl_page(&self, url: String, depth: usize) -> Result<Vec<String>> {
        if depth > self.max_depth {
            return Ok(Vec::new());
        }

        if !self.visited.insert(url.clone()) {
            return Ok(Vec::new());
        }

        if self.magnets.len() >= self.max_links {
            return Ok(Vec::new());
        }

        info!("Crawling [depth {}]: {}", depth, url);

        match self.fetch_page(&url).await {
            Ok(html) => {
                self.extract_magnets(&html);
                Ok(self.extract_links(&html, &url))
            }
            Err(e) => {
                warn!("Failed to fetch {}: {}", url, e);
                Ok(Vec::new())
            }
        }
    }

    async fn crawl(&self, start_url: String) -> Result<()> {
        let mut queue = vec![(start_url, 0)];

        while !queue.is_empty() && self.magnets.len() < self.max_links {
            let batch: Vec<_> = queue.drain(..).collect();
            
            let results = stream::iter(batch)
                .map(|(url, depth)| {
                    let scraper = self.clone_refs();
                    async move {
                        let links = scraper.crawl_page(url, depth).await.unwrap_or_default();
                        (links, depth)
                    }
                })
                .buffer_unordered(self.semaphore.available_permits().max(1))
                .collect::<Vec<_>>()
                .await;

            for (links, depth) in results {
                for link in links {
                    if self.magnets.len() >= self.max_links {
                        break;
                    }
                    queue.push((link, depth + 1));
                }
            }

            info!("Progress: {} magnets found, {} URLs visited", 
                  self.magnets.len(), self.visited.len());
        }

        Ok(())
    }

    fn clone_refs(&self) -> Self {
        Self {
            client: self.client.clone(),
            domain: self.domain.clone(),
            visited: Arc::clone(&self.visited),
            magnets: Arc::clone(&self.magnets),
            magnet_regex: self.magnet_regex.clone(),
            semaphore: Arc::clone(&self.semaphore),
            max_links: self.max_links,
            max_depth: self.max_depth,
        }
    }

    fn save_results(&self, output_path: &str) -> Result<()> {
        let torrents: Vec<String> = self.magnets.iter().map(|m| m.clone()).collect();
        let data = TorrentData { torrents };
        
        let json = serde_json::to_string_pretty(&data)?;
        fs::write(output_path, json)
            .context(format!("Failed to write to {}", output_path))?;
        
        info!("Saved {} magnet links to {}", data.torrents.len(), output_path);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    
    info!("Starting magnet link extraction from domain: {}", args.domain);
    info!("Max links: {}, Concurrent: {}, Max depth: {}", 
          args.max_links, args.concurrent, args.max_depth);

    let scraper = Scraper::new(&args)?;
    
    // Try both http and https
    let start_url = if args.domain.starts_with("http") {
        args.domain.clone()
    } else {
        format!("https://{}", args.domain)
    };

    scraper.crawl(start_url).await?;
    scraper.save_results(&args.output)?;

    info!("Extraction complete! Found {} magnet links", scraper.magnets.len());
    
    Ok(())
}