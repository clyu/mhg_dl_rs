use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, InvalidHeaderValue},
};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json;
use lz_string;
use regex::Regex;
use rand::Rng;
use std::{
    fs,
    io::{self, Write},
    num::ParseIntError,
    path::PathBuf,
    thread,
    time::Duration,
};
use thiserror::Error;
use zip::{result::ZipError, write::FileOptions, CompressionMethod, ZipWriter};
use once_cell::sync::Lazy;

static RE_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:https?://(?:[\w\.]+\.)?manhuagui\.com/comic/)?(\d+)").unwrap());
static RE_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\w+\b").unwrap());
static RE_JSON: Lazy<Regex> = Lazy::new(|| Regex::new(r".*\((\{.*\})\).*").unwrap());
static RE_CHAPTER_DATA: Lazy<Regex> = Lazy::new(|| Regex::new(r".*}\('\s*(.*?)',(\d+),(\d+),'([\w+/=]+)'.*").unwrap());
static RE_ILLEGAL_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r##"[\/:*?"<>|]"##).unwrap());


#[derive(Error, Debug)]
enum AppError {
    #[error("Invalid manhuagui URL or ID")]
    InvalidUrl,
    #[error("Content parsing error: {0}")]
    ContentParsing(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("Network request error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Invalid HTTP header: {0}")]
    InvalidHeader(#[from] InvalidHeaderValue),
    #[error("Chapter selection parsing error: {0}")]
    RangeParse(#[from] range_parser::RangeError),
    #[error("JSON parsing error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("Integer parsing error: {0}")]
    ParseInt(#[from] ParseIntError),
    #[error("Zip error: {0}")]
    Zip(#[from] ZipError),
}

type Result<T> = std::result::Result<T, AppError>;

/// Simple Manhuagui downloader in Rust
#[derive(Parser)]
#[clap(author, version, about)]
struct Args {
    /// Manhuagui URL or numeric ID
    url: String,
    /// Tunnel line: 0=i,1=eu,2=us
    #[clap(short, long, default_value_t = 0)]
    tunnel: usize,
    /// Delay between pages in milliseconds
    #[clap(short, long, default_value_t = 1000)]
    delay_ms: u64,
    /// Output directory
    #[clap(short, long, default_value = "Downloads")]
    output_dir: String,
}

fn parse_id(s: &str) -> Option<usize> {
    RE_ID.captures(s)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

#[derive(Deserialize)]
struct ChapterStruct {
    sl: Sl,
    path: String,
    files: Vec<String>,
}

#[derive(Deserialize)]
struct Sl {
    e: serde_json::Value,
    m: String,
}

struct Comic {
    client: Client,
    host: String,
    tunnel: String,
    delay: Duration,
    title: String,
    chapters: Vec<(String, String)>,
    output_dir: String,
}

fn unpack_packed(
    frame: &str,
    a: usize,
    c: usize,
    data: Vec<String>,
) -> Result<ChapterStruct> {
    fn convert_base(mut value: usize, base: usize) -> String {
        let digits = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        if value == 0 {
            return "0".to_string();
        }
        let mut res = String::new();
        while value > 0 {
            let rem = value % base;
            res.insert(0, digits.chars().nth(rem).unwrap());
            value /= base;
        }
        res
    }
    fn encode(inner: usize, a: usize) -> String {
        if inner < a {
            if inner > 35 {
                (((inner % a) as u8 + 29) as char).to_string()
            } else {
                convert_base(inner, 36)
            }
        } else {
            let rec = encode(inner / a, a);
            let ch = if inner % a > 35 {
                ((inner % a) as u8 + 29) as char
            } else {
                convert_base(inner % a, 36).chars().next().unwrap()
            };
            format!("{}{}", rec, ch)
        }
    }
    let mut dmap = std::collections::HashMap::new();
    for i in (0..c).rev() {
        let key = encode(i, a);
        let val = if data[i].is_empty() {
            key.clone()
        } else {
            data[i].clone()
        };
        dmap.insert(key, val);
    }
    // replace encoded tokens (words) with their mapped values to reconstruct JS source
    let js = RE_WORD
        .replace_all(frame, |caps: &regex::Captures| {
            let key = caps.get(0).unwrap().as_str();
            dmap.get(key).cloned().unwrap_or_else(|| key.to_string())
        })
        .to_string();
    let caps = RE_JSON.captures(&js).ok_or_else(|| {
        AppError::ContentParsing("Could not find JSON data in unpacked script.".to_string())
    })?;
    let json_str = caps
        .get(1)
        .ok_or_else(|| {
            AppError::ContentParsing(
                "Could not extract JSON string from unpacked script.".to_string(),
            )
        })?
        .as_str();
    Ok(serde_json::from_str(json_str)?)
}

impl Comic {
    fn new(id: usize, tunnel: usize, delay_ms: u64, output_dir: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        for (key, value) in &[
            ("accept", "image/webp,image/apng,image/*,*/*;q=0.8"),
            ("accept-encoding", "gzip, deflate, br"),
            ("accept-language", "zh-TW,zh;q=0.9,en-US;q=0.8,en;q=0.7,zh-CN;q=0.6"),
            ("cache-control", "no-cache"),
            ("pragma", "no-cache"),
            ("referer", "https://www.manhuagui.com/"),
            ("sec-fetch-dest", "image"),
            ("sec-fetch-mode", "no-cors"),
            ("sec-fetch-site", "cross-site"),
            ("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/81.0.4044.129 Safari/537.36"),
        ] {
            headers.insert(*key, value.parse()?);
        }
        let client = Client::builder().default_headers(headers).build()?;
        let host = String::from("https://tw.manhuagui.com");
        let channels = ["i", "eu", "us"];
        let tn = channels.get(tunnel).unwrap_or(&"i");
        let tunnel_url = format!("https://{}.hamreus.com", tn);
        let mut c = Comic {
            client,
            host,
            tunnel: tunnel_url,
            delay: Duration::from_millis(delay_ms),
            title: String::new(),
            chapters: Vec::new(),
            output_dir,
        };
        c.load_metadata(id)?;
        Ok(c)
    }

    fn load_metadata(&mut self, id: usize) -> Result<()> {
        let url = format!("{}/comic/{}", self.host, id);
        let res = self.client.get(&url).send()?.text()?;
        let document = Html::parse_document(&res);
        let sel_title = Selector::parse(".book-title h1")
            .map_err(|e| AppError::ContentParsing(format!("Failed to parse title selector: {:?}", e)))?;
        self.title = document
            .select(&sel_title)
            .next()
            .map(|e| e.text().collect::<String>())
            .ok_or_else(|| AppError::ContentParsing(format!("Could not find title for comic {}", id)))?;
        let sel_chap = Selector::parse(".chapter-list ul a")
            .map_err(|e| AppError::ContentParsing(format!("Failed to parse chapter selector: {:?}", e)))?;
        let elements: Vec<_> = document.select(&sel_chap).collect();
        for element in elements.into_iter().rev() {
            let name = element
                .value()
                .attr("title")
                .ok_or_else(|| AppError::ContentParsing("Chapter title attribute not found".to_string()))?
                .to_string();
            let href = element
                .value()
                .attr("href")
                .ok_or_else(|| AppError::ContentParsing("Chapter href attribute not found".to_string()))?
                .to_string();
            self.chapters.push((name, href));
        }
        Ok(())
    }

    fn get_chapter(&self, href: &str) -> Result<ChapterStruct> {
        let url = format!("{}{}", self.host, href);
        let text = self.client.get(&url).send()?.text()?;
        let caps = RE_CHAPTER_DATA
            .captures(&text)
            .ok_or_else(|| AppError::ContentParsing(format!("Could not parse chapter data from {}", url)))?;

        let get_cap = |i, name| {
            caps.get(i)
                .map(|m| m.as_str())
                .ok_or_else(|| AppError::ContentParsing(format!("Could not find capture group '{}' in chapter data", name)))
        };

        let frame = get_cap(1, "frame")?;
        let a: usize = get_cap(2, "a")?.parse()?;
        let c: usize = get_cap(3, "c")?.parse()?;
        let data_b64 = get_cap(4, "data_b64")?;

        let data_dec = lz_string::Decoder::new()
            .decode_base64(data_b64)
            .map_err(|_| AppError::ContentParsing("Failed to decode base64 chapter data".to_string()))?;
        let data = data_dec.split('|').map(|s| s.to_string()).collect();
        unpack_packed(frame, a, c, data)
    }

    fn download_images(&self, chap: &ChapterStruct, chapter_dir: &PathBuf, bar: &ProgressBar) -> Result<()> {
        for (i, file) in chap.files.iter().enumerate() {
            let url = format!("{}{}{}", self.tunnel, chap.path, file);
            let dst = chapter_dir.join(format!("{}_{}", i, file));
            let dst_part = PathBuf::from(format!("{}.part", dst.display()));
            if dst.exists() {
                bar.inc(1);
                continue;
            }
            let e_str = match &chap.sl.e {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => return Err(AppError::ContentParsing("sl.e is not a string or number".to_string())),
            };
            let mut resp = self
                .client
                .get(&url)
                .query(&[("e", &e_str), ("m", &chap.sl.m)])
                .send()?;
            let mut out = fs::File::create(&dst_part)?;
            io::copy(&mut resp, &mut out)?;
            fs::rename(&dst_part, &dst)?;
            thread::sleep(rand::rng().random_range(self.delay / 2..=self.delay * 3 / 2));
            bar.inc(1);
        }
        Ok(())
    }

    fn compress_chapter(&self, chapter_dir: &PathBuf, zip_path: &PathBuf) -> Result<()> {
        let zip_file = fs::File::create(&zip_path)?;
        let mut zip = ZipWriter::new(zip_file);
        let options = FileOptions::default().compression_method(CompressionMethod::Stored);
        for entry in fs::read_dir(&chapter_dir)? {
            let e = entry?;
            let path = e.path();
            if path.is_file() {
                let name = path.file_name().unwrap().to_string_lossy();
                zip.start_file(name, options)?;
                let data = fs::read(&path)?;
                zip.write_all(&data)?;
                fs::remove_file(&path)?;
            }
        }
        zip.finish()?;
        fs::remove_dir(&chapter_dir)?;
        Ok(())
    }

    fn download_chapter(&self, index: usize) -> Result<()> {
        let (ref name, ref href) = self.chapters[index];
        let book_safe = RE_ILLEGAL_CHARS.replace_all(&self.title, "_");
        let chap_safe = RE_ILLEGAL_CHARS.replace_all(&name, "_");
        let book_dir = PathBuf::from(&self.output_dir).join(book_safe.as_ref());
        let zip_path = book_dir.join(format!("{}.zip", chap_safe));
        if zip_path.exists() {
            println!("{} already exists, skipping.", zip_path.display());
            return Ok(());
        }
        let chap = self.get_chapter(href)?;
        let chapter_dir = book_dir.join(chap_safe.as_ref());
        fs::create_dir_all(&chapter_dir)?;
        let bar = ProgressBar::new(chap.files.len() as u64);
        bar.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
                )
                .unwrap() // This unwrap is on ProgressStyle, which is safe if the template is valid
                .progress_chars("#>-"),
        );
        bar.set_message(name.clone());

        self.download_images(&chap, &chapter_dir, &bar)?;
        self.compress_chapter(&chapter_dir, &zip_path)?;

        Ok(())
    }
}

fn prompt_for_chapters(chapters_count: usize) -> Result<impl Iterator<Item = usize>> {
    loop {
        print!("Select chapters (e.g. 1-3,5): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match range_parser::parse(input.trim()) {
            Ok(mut parsed_ranges) => {
                parsed_ranges.sort();
                if let Some(&last) = parsed_ranges.last() {
                    if last >= chapters_count {
                        eprintln!(
                            "Error: Chapter index {} is out of bounds. Total chapters: {}. Please enter again.",
                            last, chapters_count
                        );
                        continue;
                    }
                }
                return Ok(parsed_ranges.into_iter());
            }
            Err(_) => {
                eprintln!("Invalid input format. Please enter again.");
                continue;
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let id = parse_id(&args.url).ok_or(AppError::InvalidUrl)?;
    let comic = Comic::new(
        id,
        args.tunnel,
        args.delay_ms,
        args.output_dir,
    )?;
    println!("Title: {}", comic.title);
    for (i, (name, _)) in comic.chapters.iter().enumerate() {
        println!("{}: {}", i, name);
    }

    let mut ranges = prompt_for_chapters(comic.chapters.len())?.peekable();

    while let Some(idx) = ranges.next() {
        if let Err(e) = comic.download_chapter(idx) {
            eprintln!("Failed to download chapter {}: {}", idx, e);
        }
        if ranges.peek().is_some() {
            thread::sleep(Duration::from_secs(5));
        }
    }
    Ok(())
}
