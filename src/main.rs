use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue},
    Url,
};
use scraper::{Html, Selector};
use serde::Deserialize;
use regex::Regex;
use rand::Rng;
use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    num::ParseIntError,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::LazyLock,
    thread,
    time::Duration,
};
use thiserror::Error;
use zip::{result::ZipError, write::FileOptions, CompressionMethod, ZipWriter};

const HOST: &str = "https://tw.manhuagui.com";
const TUNNEL_CHANNELS: [&str; 3] = ["i", "eu", "us"];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `HOST` parsed once, as the base every site-relative link is resolved against.
/// Parsing also normalizes it to a trailing slash, which is what the site root
/// referer needs to be.
static HOST_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(HOST).expect("HOST is a valid absolute URL"));

/// `--tunnel` help text, generated from `TUNNEL_CHANNELS` so the option's
/// documentation cannot drift when the channel list changes.
static TUNNEL_HELP: LazyLock<String> = LazyLock::new(|| {
    let channels = TUNNEL_CHANNELS
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{i}={c}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("Tunnel line: {channels}")
});

/// A comic ID, from a bare number or from a URL carrying one. The classes are
/// spelled out rather than written `\w`/`\d`, which this crate reads as
/// Unicode: a host label and a comic ID are ASCII by definition, and a
/// full-width digit never parsed into a `usize` anyway.
///
/// `\b` is deliberately left Unicode-aware. It is what keeps an ID with text
/// glued to it from being accepted, and CJK is text: there is no boundary
/// between the digits and the CJK in `12345漢`, so that input does not parse —
/// an ASCII `\b` would find one there and hand back 12345.
static RE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:https?://(?:[0-9A-Za-z_.]+\.)?manhuagui\.com)?/comic/)?([0-9]+)\b")
        .unwrap()
});
/// One dictionary key of a packed script. No `\b` anchors are needed: a scan
/// takes the leftmost match, so `\w+` can only start where a run of word
/// characters starts, and being greedy it always consumes the run to its end.
///
/// `(?-u:…)` is what makes that argument hold against the packer, whose word
/// class is JavaScript's `[0-9A-Za-z_]`; this crate's `\w` is Unicode-aware and
/// counts CJK as word characters. The frame keeps every non-word character of
/// the original source verbatim, so a key can end up written straight against
/// text JavaScript treated as a separator — `第5話`, where only the `5` is the
/// key. Unicode semantics would take that whole run as a single word, match it
/// against no key, and hand the key back unsubstituted.
static RE_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\w+)").unwrap());
/// The packed-script call: frame, base, dictionary size, lz-string payload.
/// The digits and the base64 alphabet are ASCII for the reason `RE_ID`'s are.
/// `\s*` is left alone — Unicode space after the quote is as good a match as
/// ASCII space — and `.` cannot be narrowed at all: outside Unicode mode it
/// matches bytes, which the crate rejects in a pattern over `&str`.
///
/// The frame capture is the raw text of a single-quoted JS string literal, so
/// it still carries the packer's `\'` and `\\` escapes;
/// `unescape_packed_frame` is what undoes them before the frame is used.
static RE_CHAPTER_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"}\('\s*(.*?)',([0-9]+),([0-9]+),'([0-9A-Za-z_+/=]+)'").unwrap());

static SEL_COMICS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.book-result ul li.cf").unwrap());
static SEL_LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.bcover").unwrap());
static SEL_TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".book-title h1").unwrap());
/// A chapter link inside a `.chapter-list`. Both attributes are required by the
/// selector so that non-chapter anchors — the `<a id="v1" href="javascript:;">`
/// pager entries that sit in a sibling block today, ad links, "more" links — are
/// simply not matched, rather than aborting the whole book's parse.
static SEL_CHAPTER_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href][title]").unwrap());
static SEL_PAGER_LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.pager a").unwrap());
static SEL_VIEWSTATE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("input#__VIEWSTATE").unwrap());
static SEL_CHAPTER_LIST: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".chapter-list").unwrap());

/// Per-chapter download bar. The template is fixed, so parse it once instead of
/// re-parsing (and re-`unwrap`ping) it for every chapter.
static BAR_STYLE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::default_bar()
        .template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}",
        )
        .unwrap()
        .progress_chars("#>-")
});

#[derive(Error, Debug)]
enum AppError {
    #[error("Invalid manhuagui URL or ID")]
    InvalidUrl,
    #[error("Interrupted by Ctrl+C")]
    Interrupted,
    #[error("No comics found for '{0}'")]
    NoSearchResults(String),
    #[error("Content parsing error: {0}")]
    ContentParsing(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("Network request error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("JSON parsing error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("Integer parsing error: {0}")]
    ParseInt(#[from] ParseIntError),
    #[error("Zip error: {0}")]
    Zip(#[from] ZipError),
}

impl AppError {
    /// Everything a page can get wrong ends up in `ContentParsing`, built by
    /// hand from a literal or a `format!`. `Into<String>` takes both, which is
    /// what keeps a `.to_string()` off every call site that passes a literal.
    fn parsing(msg: impl Into<String>) -> Self {
        AppError::ContentParsing(msg.into())
    }
}

type Result<T> = std::result::Result<T, AppError>;

/// Simple Manhuagui downloader in Rust
#[derive(Parser)]
#[clap(author, version, about)]
struct Args {
    /// Manhuagui URL or numeric ID
    #[clap(value_name = "URL", required_unless_present = "search", conflicts_with = "search")]
    url: Option<String>,
    /// Search keyword for comics
    #[clap(short, long)]
    search: Option<String>,
    #[clap(short, long, default_value_t = 0, help = TUNNEL_HELP.as_str(), value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(..TUNNEL_CHANNELS.len() as u64))]
    tunnel: usize,
    /// Delay between pages in milliseconds
    #[clap(short, long, default_value_t = 1000)]
    delay_ms: u64,
    /// Output directory
    #[clap(short, long, default_value = "Downloads")]
    output_dir: PathBuf,
}

/// Extract a comic ID from a bare number, an absolute manhuagui comic URL,
/// or a site-relative path like `/comic/12345/` (as found in search results).
///
/// The input is trimmed first, because `RE_ID` is anchored at the start of the
/// string: a pasted URL carrying a leading space, or an `href` attribute the
/// page wrote with surrounding whitespace (which a browser strips and the HTML
/// parser does not), would otherwise not match at all.
fn parse_id(s: &str) -> Option<usize> {
    RE_ID.captures(s.trim()).and_then(|c| c[1].parse().ok())
}

#[derive(Deserialize, Debug)]
struct ChapterStruct {
    sl: Sl,
    path: String,
    files: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct Sl {
    e: NumOrStr,
    m: String,
}

/// `sl.e` appears as either a number or a string in chapter data; accept both
/// and reject anything else at deserialization time.
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum NumOrStr {
    Num(serde_json::Number),
    Str(String),
}

impl std::fmt::Display for NumOrStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NumOrStr::Num(n) => n.fmt(f),
            NumOrStr::Str(s) => s.fmt(f),
        }
    }
}

#[derive(Debug)]
struct SearchResult {
    title: String,
    comic_id: usize,
}

#[derive(Debug)]
struct Chapter {
    name: String,
    href: String,
    /// Section heading the chapter belongs to, e.g. "單行本" (volumes) or "單話" (single chapters)
    group: String,
}

struct Comic {
    client: Client,
    /// Image host, parsed once so a page's `path` is resolved against it rather
    /// than concatenated onto it. See `Comic::image_url`.
    tunnel: Url,
    delay: Duration,
    title: String,
    chapters: Vec<Chapter>,
    /// Sanitized title, used as the book directory name and zip name prefix.
    book_safe: String,
    book_dir: PathBuf,
}

/// The `.part` file `write_atomic` builds its output in, and the only thing
/// that constructs one — go through `write_atomic` rather than driving this
/// directly.
///
/// Dropping the guard without a successful `commit` deletes the partial file.
/// Without that, every *unwinding* exit between creating it and the final
/// rename — a stalled transfer, a short read, a page missing from disk, the
/// rename itself — would leave a stray `.part` behind, in a directory that is
/// otherwise only cleaned up on the success path.
///
/// Ctrl+C during a download is the exception, and it is not covered: SIGINT
/// ends the process without running `Drop`, so that `.part` does survive. It is
/// inert — `create` truncates a leftover, and `compress_chapter` packs the page
/// list `download_images` returned rather than the directory, so a stray
/// `.part` can neither be resumed from nor reach the archive.
///
/// Inert is not the same as gone, and the two leftovers are not swept alike. A
/// page's `.part` sits in the chapter directory, which `compress_chapter`
/// removes whole once the archive is written. The archive's own
/// `{book}_{chapter}.cbz.part` sits in the book directory, among the finished
/// .cbz files, and nothing sweeps that one: `download_chapter` tests for the
/// .cbz and not the `.part`, so it is truncated only by compressing that same
/// chapter again. Interrupt ten different chapters mid-zip and ten of them are
/// still in the library afterwards.
struct PartFile {
    part: PathBuf,
    dst: PathBuf,
    committed: bool,
}

impl PartFile {
    /// Create `dst` with `.part` appended, truncating any leftover from an
    /// earlier run, and hand back the guard together with the open handle.
    fn create(dst: &Path) -> Result<(Self, fs::File)> {
        let mut part = dst.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let file = fs::File::create(&part)?;
        Ok((
            PartFile {
                part,
                dst: dst.to_path_buf(),
                committed: false,
            },
            file,
        ))
    }

    /// Move the finished file into place. The handle it was written through
    /// must be closed first: Windows can refuse to move a file that is still
    /// open. A failed rename leaves the guard armed, so the partial file is
    /// removed on the way out.
    fn commit(mut self) -> Result<()> {
        fs::rename(&self.part, &self.dst)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.part);
        }
    }
}

/// Produce `dst` through a `.part` file, so its final name only ever appears
/// once the content behind it is complete: `write` fills the handle, the handle
/// is closed, and only then is the file renamed into place. Anything `write`
/// rejects — a short read, a page missing from disk — must be reported from
/// inside it, because returning `Ok` is what publishes the file.
fn write_atomic(dst: &Path, write: impl FnOnce(&mut fs::File) -> Result<()>) -> Result<()> {
    let (part, mut file) = PartFile::create(dst)?;
    write(&mut file)?;
    // Close the handle before the rename: Windows can refuse to move a file
    // that is still open. `file` is bound after `part`, so on the error path
    // above it is dropped — and closed — first as well, before `part` removes
    // the partial file.
    drop(file);
    part.commit()
}

/// How many bytes of a response `looks_like_image` needs to see. The longest
/// signature it checks is WebP's, whose `WEBP` tag sits at offset 8.
const IMAGE_HEAD_LEN: usize = 12;

/// Whether `head` — the first `IMAGE_HEAD_LEN` bytes of a response — opens with
/// the signature of one of the two formats the tunnel serves.
///
/// This is an allow list rather than a "does not look like HTML" test: what has
/// to be kept out is anything that is *not* an image, and an anti-hotlink or
/// interstitial response answering 200 need not be HTML for `error_for_status`
/// to have let it through. The cost of a list this narrow is that a format the
/// site adds later is rejected until it is added here — which surfaces as the
/// caller's error, naming the bytes it saw, rather than as a corrupt page
/// sealed inside a finished .cbz.
fn looks_like_image(head: &[u8]) -> bool {
    head.starts_with(b"\xFF\xD8\xFF")                                       // JPEG
        || (head.starts_with(b"RIFF") && head.get(8..12) == Some(&b"WEBP"[..])) // WebP
}

fn unpack_packed(
    frame: &str,
    a: usize,
    c: usize,
    data: &[&str],
) -> Result<ChapterStruct> {
    const DIGITS: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    // The dictionary key the packed script gives word number `value`: its
    // digits in base `base`, most significant first. Relies on the base having
    // been validated once up front.
    fn encode(mut value: usize, base: usize) -> String {
        // Division hands the digits back least significant first, so they are
        // collected that way and flipped once at the end — inserting each at
        // the front instead would shift everything collected so far. Dividing
        // before testing `value` is also what gives `encode(0, _)` its "0",
        // with no special case of its own.
        let mut digits = Vec::new();
        loop {
            digits.push(DIGITS.as_bytes()[value % base]);
            value /= base;
            if value == 0 {
                break;
            }
        }
        digits.reverse();
        // Every byte was indexed out of `DIGITS`, which is ASCII throughout, so
        // this hands the buffer over as the string rather than copying it.
        String::from_utf8(digits).expect("DIGITS is ASCII")
    }
    if a < 2 || a > DIGITS.len() {
        return Err(AppError::parsing(format!(
            "Base {} out of supported range (2..={})",
            a,
            DIGITS.len()
        )));
    }
    if c > data.len() {
        return Err(AppError::parsing(format!(
            "Packed script dictionary size mismatch: expected {} words, got {}",
            c,
            data.len()
        )));
    }
    // Sized only after the check above, so a bogus `c` from a hostile page
    // cannot ask for a huge allocation up front.
    let mut dmap = HashMap::with_capacity(c);
    // `take(c)` is what holds the dictionary to the size the packed script
    // declared, and it does so without indexing: an oversized `c` could not
    // read out of bounds even unchecked, it would only yield a dictionary
    // shorter than asked for. So the check above is not standing between a
    // hostile page and a panic; it buys two other things. It bounds the
    // allocation on the line above, and it names the fault, because a
    // dictionary that falls short leaves every word from `data.len()` on
    // unsubstituted — which would otherwise surface as an unintelligible
    // complaint about a script that has no JSON in it.
    for (i, word) in data.iter().take(c).enumerate() {
        // An empty dictionary entry maps the word to itself, which is also
        // what the replacement below does for unknown words — skip it before
        // paying for the `encode` allocation.
        if !word.is_empty() {
            dmap.insert(encode(i, a), *word);
        }
    }
    // Written out by hand rather than with `replace_all`, whose `Replacer`
    // return type cannot borrow from the match: the closure would have to hand
    // back an owned `String` for every word in the frame, including the many it
    // leaves alone, which is thousands of allocations for a page. Copying the
    // gaps and the words straight into the output costs one allocation.
    let mut js = String::with_capacity(frame.len());
    let mut last = 0;
    for m in RE_WORD.find_iter(frame) {
        js.push_str(&frame[last..m.start()]);
        js.push_str(dmap.get(m.as_str()).copied().unwrap_or(m.as_str()));
        last = m.end();
    }
    js.push_str(&frame[last..]);
    // The payload is the argument of the first `({ … })` call in the unpacked
    // script. Only its start is searched for; its end comes from deserializing
    // exactly one JSON value, not from matching a closing `})`. The packed
    // frame keeps every non-word character verbatim, so a `})` sitting inside a
    // free-text field — `bname` and `cname` carry comic and chapter titles —
    // reaches the payload unescaped, and a regex ending at the first `})` would
    // cut the object short there and report it as an unintelligible "EOF while
    // parsing" from serde.
    let no_json = || AppError::parsing("Could not find JSON data in unpacked script.");
    // `find` returns the offset of `(`; the value starts at the `{` after it.
    let start = js.find("({").ok_or_else(no_json)? + 1;
    let chapter: ChapterStruct = serde_json::Deserializer::from_str(&js[start..])
        .into_iter()
        .next()
        .transpose()?
        .ok_or_else(no_json)?;
    // A chapter with no images would produce an empty .cbz that marks the
    // chapter as done forever; fail here so the user sees an error instead.
    if chapter.files.is_empty() {
        return Err(AppError::parsing("Chapter data contains no image files"));
    }
    Ok(chapter)
}

/// Undo the escaping the packer applied when it wrote the frame into the page
/// as a single-quoted JS string literal. A browser never sees the problem:
/// `eval` decodes the literal before the unpacker runs. `RE_CHAPTER_DATA`
/// reads the literal's raw text instead, so a quote in a comic or chapter
/// title reaches serde still escaped — `'` as `\'`, which is not a JSON
/// escape at all, and the backslash of a JSON `\"` as `\\`, whose first half
/// serde takes as a complete escape, leaving the quote to end the string
/// early. Either way the whole chapter fails to parse, even though the title
/// fields are never deserialized: serde still has to lex past them.
///
/// Only `\'` and `\\` are decoded, because they are everything the packer's
/// escaper writes (`p.replace(/([\\'])/g, '\\$1')`): every backslash in a
/// well-formed frame opens one of the two. JSON's own escapes ride through
/// unchanged — a title's newline travels as `\\n` and comes back `\n`, which
/// is serde's to decode. Any other `\x` is not this packer's output, so it is
/// kept verbatim rather than guessed at.
fn unescape_packed_frame(frame: &str) -> Cow<'_, str> {
    // Escape-free frames are the overwhelmingly common case — hand the input
    // back rather than copying it.
    if !frame.contains('\\') {
        return Cow::Borrowed(frame);
    }
    let mut out = String::with_capacity(frame.len());
    let mut chars = frame.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped @ ('\\' | '\'')) => out.push(escaped),
                Some(other) => {
                    out.push(c);
                    out.push(other);
                }
                None => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Byte budget for one sanitized name.
///
/// The limit that actually matters belongs to the file system — 255 bytes per
/// path component on ext4 and APFS, 255 UTF-16 units on NTFS — and the longest
/// component this program builds is a chapter's archive *while it is still
/// being written*, `{book}_{chapter}.cbz.part`: two sanitized names at once,
/// plus the suffix `write_atomic` appends. Budgeting each at 120 keeps that
/// under the limit (120 + 1 + 120 + 4 + 5 = 250), and 250 bytes of CJK is well
/// under 255 UTF-16 units too. A page file name, one sanitized name behind a
/// short index, is nowhere near it even with the suffix.
///
/// One name can come back a byte over the budget, when `sanitize` spends it
/// breaking up a Windows device name (see `RESERVED_DEVICE_NAMES`). Two of
/// those at once is the real worst case, 252, and the slack below absorbs it.
///
/// Counting those last 5 bytes is what pins the ceiling at 122 rather than the
/// 125 the finished name alone would allow, and the failure they buy off lands
/// on the temporary file — before the name it would be renamed to ever exists.
///
/// Without a bound a long title is not a cosmetic problem: the create fails with
/// `ENAMETOOLONG`, and it fails again on every later run, so the chapter can
/// never be downloaded at all.
const MAX_NAME_BYTES: usize = 120;

/// The names Windows resolves to a device wherever a path component is
/// expected. The match is case-insensitive, and an extension is no escape:
/// `NUL.txt` and `NUL.tar.gz` open the null device exactly as `NUL` does, which
/// is why `sanitize` tests the stem rather than the whole name.
///
/// The failure they cause is the one `MAX_NAME_BYTES` exists to prevent, in a
/// different disguise: `create_dir_all` on a book or chapter named `NUL` fails,
/// and it fails again on every later run, so that comic can never be downloaded
/// at all. The CI matrix builds `x86_64-pc-windows-msvc`, so this is a platform
/// the program ships on.
const RESERVED_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
    "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
    "LPT7", "LPT8", "LPT9",
];

/// The printable characters Windows forbids in a file name. The rest of what
/// it forbids is the C0 controls and DEL, which `sanitize` tests for with
/// `char::is_ascii_control` instead of spelling out here.
const ILLEGAL_NAME_CHARS: &str = r#"\/:*?"<>|"#;

/// Make `s` usable as a single path component: strip surrounding whitespace,
/// replace characters that are invalid in file names with `_`, cut it down to
/// `MAX_NAME_BYTES` without leaving a trailing dot or space, and break up a
/// Windows device name.
/// Windows rejects names ending in a dot or a space, and a name consisting only
/// of dots (`.`, `..`) would otherwise resolve to a directory outside the
/// intended one. Falls back to `_` when nothing usable is left.
///
/// Two titles sharing a truncated prefix collapse onto one name, as two titles
/// differing only in an illegal character already do. `download_chapter` treats
/// an existing archive as done, so a collision shows up as a chapter skipped
/// rather than as one overwriting another.
fn sanitize(s: &str) -> String {
    // Trim before replacing: `\t`, `\n` and `\r` are whitespace and illegal at
    // once, and replaced first they would become `_`s that no trim removes —
    // a title ending in a newline would keep an underscore it never showed.
    // Replacing cannot put whitespace back at either end, so one trim is enough.
    let cleaned = s
        .trim()
        .replace(|c: char| c.is_ascii_control() || ILLEGAL_NAME_CHARS.contains(c), "_");
    // Cut at the last char boundary at or below the budget, since slicing a
    // multi-byte character in half would panic. A name already within the
    // budget comes back whole: past the end, the floor is the length itself.
    let bounded = &cleaned[..cleaned.floor_char_boundary(MAX_NAME_BYTES)];
    // The only place a trailing dot or space is stripped — Windows rejects a
    // name ending in either, and the cut above can expose one that was not at
    // the end before. Trimming before the cut as well would change nothing:
    // dots and spaces are single-byte, so a trailing run of them is either cut
    // away with the rest of the overflow, or survives the cut and is removed
    // right here.
    let bounded = bounded.trim_end_matches(['.', ' ']);
    if bounded.is_empty() {
        return "_".to_string();
    }
    // Last, because both steps above can turn a name into a device name that
    // was not one on the way in: the cut drops the tail off `NUL.long…`, and
    // the trim takes the dot off `NUL.`.
    //
    // The `_` goes after the stem and not at the end of the name, which would
    // leave `NUL.txt_` — still the null device, since Windows looks only at
    // what precedes the first dot. That is also why this can hand back
    // `MAX_NAME_BYTES + 1` bytes: the budget has the room (see the 122 its
    // arithmetic allows), and cutting to make space would just restore the
    // reserved name.
    let stem = bounded.split_once('.').map_or(bounded, |(stem, _)| stem);
    if RESERVED_DEVICE_NAMES.iter().any(|name| stem.eq_ignore_ascii_case(name)) {
        return format!("{stem}_{}", &bounded[stem.len()..]);
    }
    bounded.to_string()
}

/// `None` when `data` is not lz-string base64. Whether that is an error is the
/// caller's call: the chapter data cannot do without its dictionary, while a
/// `__VIEWSTATE` that does not decode is simply not a chapter list.
fn decode_lz_base64(data: &str) -> Option<String> {
    lz_string::Decoder::new().decode_base64(data).ok()
}

/// Only the headers that are identical on *every* request belong here.
/// Anything that describes one kind of request — `accept`, `priority`, the
/// `sec-fetch-*` triple — is set at the call site instead: `fetch_html` sends
/// them as a top-level navigation, `download_images` as a cross-site image.
/// Keeping request-shaped values out of the defaults is what stops the image
/// fetch from having to override five of them to undo a document's.
fn build_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("accept-encoding", "gzip, deflate, br"),
        ("accept-language", "zh-TW;q=0.8,en-US,en;q=0.5,zh;q=0.3"),
        ("cache-control", "no-cache"),
        ("dnt", "1"),
        ("pragma", "no-cache"),
        ("sec-gpc", "1"),
        ("user-agent", "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"),
    ] {
        // Both halves are `&'static str` literals, so both conversions are
        // infallible in practice and panic on a typo rather than turning a bug
        // in this table into a runtime error the user has to interpret.
        headers.insert(key, HeaderValue::from_static(value));
    }
    // Without timeouts a connection that stalls after the handshake hangs the
    // download forever; the request timeout covers reading the response body,
    // which is where image downloads spend their time.
    Ok(Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

/// Resolve `href` against `base`. Pages are joined against the site root and
/// images against their tunnel host, but both need the same thing out of it:
/// whatever the href carries raw comes back percent-encoded, and an href that
/// is already absolute replaces the base instead of being appended to it.
fn join_url(base: &Url, href: &str) -> Result<Url> {
    base.join(href)
        .map_err(|e| AppError::parsing(format!("Invalid URL '{href}': {e}")))
}

/// Resolve a link against the site root — for the hrefs whose base *is* the
/// root: the comic landing page this program writes itself, and the chapter
/// links that hang off it.
///
/// A link read off a page whose own URL is in hand is joined against that page
/// instead; see `interactive_search`, which resolves the pager's href against
/// the result page it came from.
fn resolve_url(href: &str) -> Result<Url> {
    join_url(&HOST_URL, href)
}

/// Fetch a page the way a browser fetches a top-level document; see
/// `build_client` for why the document-shaped headers live here rather than in
/// the client defaults.
///
/// Both URLs arrive as `Url` rather than as text, which is what keeps a raw
/// href from ever reaching the `referer` header: every one of them has been
/// through `join_url`, and percent-encoding is that function's whole point.
/// A header value cannot carry the non-ASCII bytes the search pager writes.
fn fetch_html(client: &Client, url: &Url, referer: &Url) -> Result<String> {
    Ok(client
        // Cloned rather than handed over as a string: `IntoUrl` takes a `Url`
        // as it is and re-parses a `&str`.
        .get(url.clone())
        .header("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("cookie", "country=TW")
        .header("priority", "u=0, i")
        .header("referer", referer.as_str())
        .header("sec-fetch-dest", "document")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-user", "?1")
        .header("upgrade-insecure-requests", "1")
        .send()?
        .error_for_status()?
        .text()?)
}

/// Raw mode for as long as the guard lives.
///
/// The prompt below is written with `print!`, which panics when stdout cannot
/// be written to — a closed pipe is enough. Without a guard, that panic would
/// unwind straight past `disable_raw_mode` and leave raw mode on for whatever
/// the user runs in that terminal next.
struct RawMode;

impl RawMode {
    /// `None` when there is no terminal to switch into raw mode, which is the
    /// one case the caller has to report rather than recover from.
    fn enable() -> Option<Self> {
        terminal::enable_raw_mode().ok().map(|()| RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Ask whether to fetch the next page of search results, waiting for a single
/// key press in raw mode. Returns `Ok(true)` for SPACE and `Ok(false)` for any
/// other key. Raw mode swallows Ctrl+C instead of raising SIGINT, so it is
/// detected here and reported as `AppError::Interrupted`.
///
/// The prompt is drawn only once raw mode is on, so that a run with no terminal
/// to read a key from — a CI job, a redirected stdin with no controlling tty —
/// says why it is stopping. Drawing a prompt that cannot be answered and then
/// stopping anyway is indistinguishable from the search having run out of
/// results. Whether the pager works is decided by the raw mode attempt itself
/// and not by testing stdin: crossterm reads keys from the controlling terminal,
/// which is still there when only stdin has been redirected.
fn prompt_for_next_page() -> Result<bool> {
    let Some(raw) = RawMode::enable() else {
        println!("--- More results available, but there is no terminal to read a key from ---");
        return Ok(false);
    };
    print!("--- Press SPACE for next page, any other key to stop ---");
    // A prompt that fails to flush is not worth abandoning the search over: the
    // key press is readable either way.
    let _ = io::stdout().flush();
    let result = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('c' | 'C'),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            })) if modifiers.contains(KeyModifiers::CONTROL) => break Err(AppError::Interrupted),
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                kind: KeyEventKind::Press,
                ..
            })) => break Ok(true),
            Ok(Event::Key(KeyEvent {
                kind: KeyEventKind::Press,
                ..
            })) => break Ok(false),
            Err(_) => break Ok(false),
            _ => {}
        }
    };
    // Drop whatever arrived together with the key that ended the wait — the
    // rest of a paste, say — while raw mode still owns the input, so that a
    // stale key cannot answer the next page prompt on its own. A zero timeout
    // only takes what has already arrived, so this never waits, and a key
    // pressed after this point is not caught. The usual one is the Enter a
    // user types after "q", which reaches the selection prompt as a blank
    // line; `prompt_until_valid` is what ignores it.
    while event::poll(Duration::ZERO).unwrap_or(false) && event::read().is_ok() {}
    // Leave raw mode before the newline, not after: in raw mode a newline moves
    // down without returning to column 0, so the line following this prompt
    // would start under its right-hand end. Dropping the guard by hand is what
    // orders the two — letting it fall off the end of the function instead
    // would put the newline first.
    drop(raw);
    println!();
    result
}

fn search_result_from_item(li: scraper::ElementRef<'_>) -> Option<SearchResult> {
    let link = li.select(&SEL_LINK).next()?;
    let comic_id = parse_id(link.value().attr("href")?)?;
    let title = link.value().attr("title")?;
    Some(SearchResult {
        title: title.to_string(),
        comic_id,
    })
}

/// Extract one page of search hits plus the href of the "next page" pager link,
/// if the page has one that leads to a page. A page with no recognizable
/// results is not an error here — `interactive_search` stops paging when it
/// sees one, and reports the search as empty only if no page yielded anything
/// at all.
fn parse_search_results(html: &str) -> (Vec<SearchResult>, Option<String>) {
    let document = Html::parse_document(html);

    let results: Vec<SearchResult> = document
        .select(&SEL_COMICS)
        .filter_map(search_result_from_item)
        .collect();

    let next_page = document.select(&SEL_PAGER_LINKS)
        .find(|a| a.text().collect::<String>().trim() == "下一頁")
        .and_then(|a| a.value().attr("href"))
        // A pager can disable its "next" link by pointing it at `javascript:;`
        // rather than leaving it out. That is the last page, and has to read as
        // one here: passed on, it would have the user press SPACE for a page
        // that reqwest then refuses to request.
        //
        // Joining against the site root rather than the page's own URL is
        // enough to learn the scheme: an href without one inherits the base's,
        // and every page this runs on was itself fetched over http(s). An href
        // that does not parse at all is kept, so that it goes on to fail where
        // `interactive_search` resolves it, with a warning that names it.
        .filter(|href| {
            HOST_URL
                .join(href)
                .map_or(true, |url| matches!(url.scheme(), "http" | "https"))
        })
        .map(|s| s.to_string());

    (results, next_page)
}

/// The section heading of a chapter list is the nearest `h4` among its
/// preceding siblings (other elements like the pager or tip blocks may sit
/// in between).
fn group_for_list(list_elem: scraper::ElementRef<'_>) -> String {
    list_elem
        .prev_siblings()
        .filter_map(scraper::ElementRef::wrap)
        .find(|e| e.value().name() == "h4")
        .map(|h| h.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Chapters".to_string())
}

/// Collect every chapter, tagged with the section heading it sits under.
///
/// A `.chapter-list` holds one `<ul>` per pager page (only the last one is
/// rendered with `style="display:block"`; the pager swaps between them in the
/// browser), and the two nesting levels are ordered *differently*: the `ul`s
/// run oldest block first, while the entries inside each `ul` run newest
/// first. comic_1128's 單行本 section is
/// `[ul(第22卷 … 第01卷), ul(第112卷 … 第23卷)]`, so reversing each `ul` on its
/// own while keeping the `ul` order yields a continuous 第01卷 … 第112卷.
///
/// Do not collapse this into a single `ul a` selector and do not hoist the
/// reverse up to the whole `.chapter-list` — either change silently scrambles
/// chapter order across pager boundaries. For the same reason the `ul`s are
/// taken as direct children rather than with a descendant selector: a nested
/// `ul` would otherwise be visited twice, once through its parent block and
/// once as a block of its own, duplicating its links and splitting the
/// enclosing block's reverse into pieces.
fn extract_chapters_with_groups(document: &Html) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    for list_elem in document.select(&SEL_CHAPTER_LIST) {
        let group = group_for_list(list_elem);
        let uls = list_elem
            .children()
            .filter_map(scraper::ElementRef::wrap)
            .filter(|e| e.value().name() == "ul");
        for ul_elem in uls {
            let start = chapters.len();
            chapters.extend(ul_elem.select(&SEL_CHAPTER_LINK).filter_map(|link_elem| {
                let link = link_elem.value();
                Some(Chapter {
                    // Trimmed like the book title: the parser keeps an
                    // attribute's padding, and the name is printed in the
                    // listing and on the progress bar as it is stored here.
                    name: link.attr("title")?.trim().to_string(),
                    href: link.attr("href")?.to_string(),
                    group: group.clone(),
                })
            }));
            // Entries within one `ul` are listed newest-first; reverse into
            // reading order. Reversing just this `ul`'s slice is what keeps
            // the pager blocks in the order described above.
            chapters[start..].reverse();
        }
    }

    chapters
}

fn parse_comic_html(html: &str) -> Result<(String, Vec<Chapter>)> {
    let document = Html::parse_document(html);
    let title = document
        .select(&SEL_TITLE)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::parsing("Could not find title"))?;

    let mut chapters = extract_chapters_with_groups(&document);

    // A gated page ships the real chapter list in the __VIEWSTATE blob.
    // A decode failure must not abort the parse: the input may be an
    // unrelated ASP.NET view state that merely happens to use that id, and
    // the "no chapters" error below describes the situation far better
    // than a decoder complaint would.
    if chapters.is_empty()
        && let Some(decoded) = document
            .select(&SEL_VIEWSTATE)
            .next()
            .and_then(|e| e.value().attr("value"))
            .and_then(decode_lz_base64)
    {
        chapters = extract_chapters_with_groups(&Html::parse_fragment(&decoded));
    }

    if chapters.is_empty() {
        return Err(AppError::parsing(
            "No chapters found (page layout changed or content is gated)",
        ));
    }

    Ok((title, chapters))
}

/// The seam the chapter tests drive: fetching is `download_chapter`'s job,
/// so everything below this line works off page text alone.
fn parse_chapter_html(html: &str) -> Result<ChapterStruct> {
    let caps = RE_CHAPTER_DATA
        .captures(html)
        .ok_or_else(|| AppError::parsing("Could not parse chapter data"))?;

    let frame = unescape_packed_frame(&caps[1]);
    let a: usize = caps[2].parse()?;
    let c: usize = caps[3].parse()?;
    let data_b64 = &caps[4];

    let data_dec = decode_lz_base64(data_b64)
        .ok_or_else(|| AppError::parsing("Failed to decode base64 chapter data"))?;
    let data: Vec<&str> = data_dec.split('|').collect();
    unpack_packed(&frame, a, c, &data)
}

impl Comic {
    /// Fetch a comic's landing page and build the download context around it.
    ///
    /// `id` stays a separate argument rather than being read off `args`: it may
    /// have come from `--search` instead of the positional URL.
    fn new(id: usize, client: Client, args: &Args) -> Result<Self> {
        let url = resolve_url(&format!("/comic/{id}"))?;
        let res = fetch_html(&client, &url, &HOST_URL)?;
        let (title, chapters) = parse_comic_html(&res)?;
        let book_safe = sanitize(&title);
        let book_dir = args.output_dir.join(&book_safe);
        Ok(Comic {
            client,
            // Built from a static channel name, so this parses or the channel
            // table is wrong — a bad base is a bug here, not a page's doing,
            // and failing per image request would report it 48 times over.
            tunnel: Url::parse(&format!("https://{}.hamreus.com", TUNNEL_CHANNELS[args.tunnel]))
                .expect("TUNNEL_CHANNELS entries form valid absolute URLs"),
            delay: Duration::from_millis(args.delay_ms),
            title,
            chapters,
            book_safe,
            book_dir,
        })
    }

    /// The tunnel URL of a single page.
    ///
    /// `path` and `file` are read out of the chapter's JSON, so they are joined
    /// onto the tunnel through `Url` rather than concatenated as text: a `path`
    /// arriving without its leading slash would otherwise be glued straight onto
    /// the host name, turning every page of the chapter into a DNS failure that
    /// names a host nobody wrote.
    fn image_url(&self, path: &str, file: &str) -> Result<Url> {
        // Percent-encode the three characters that would otherwise steer the
        // resolve instead of naming a file, in one pass over both halves.
        //
        // `?` and `#` end the path: everything after one becomes a query or a
        // fragment, so a page could hand back a URL that requests a directory
        // it did not name. `\` is the one that matters, because it defeats the
        // guard below rather than merely truncating: `Url` follows the WHATWG
        // rules, where a backslash is a path separator for special schemes, so
        // `/\evil.example/x` reaches the same authority-relative state `//`
        // does — and the leading slash this function anchors is what supplies
        // the first half of it.
        //
        // Nothing else is touched. Raw UTF-8 is exactly what `join_url` is
        // there to encode, an existing `%XX` has to survive unchanged, and a
        // `..` segment is `Url`'s to resolve — against a base whose host it
        // cannot climb past anyway.
        let mut joined = String::with_capacity(path.len() + file.len());
        for c in path.chars().chain(file.chars()) {
            match c {
                '?' => joined.push_str("%3F"),
                '#' => joined.push_str("%23"),
                '\\' => joined.push_str("%5C"),
                _ => joined.push(c),
            }
        }
        // Anchored at exactly one leading slash, which is what makes this an
        // absolute path on the tunnel host no matter what the page supplied:
        // none would resolve relative to the base, and two would make it
        // protocol-relative, handing the request to a host of the page's
        // choosing.
        join_url(&self.tunnel, &format!("/{}", joined.trim_start_matches('/')))
    }

    /// Download every page of `chap` into `chapter_dir`, skipping pages that are
    /// already there. Returns the page file names in reading order, which is what
    /// `compress_chapter` packs.
    ///
    /// `chapter_url` is the page these images hang off, sent back as the
    /// `referer` the tunnel checks. It arrives as a `Url` for the reason
    /// `fetch_html`'s referer does: only `join_url`'s output reaches a header
    /// value, and a header value cannot carry raw non-ASCII bytes.
    fn download_images(
        &self,
        chap: &ChapterStruct,
        chapter_dir: &Path,
        bar: &ProgressBar,
        chapter_url: &Url,
    ) -> Result<Vec<String>> {
        let width = chap.files.len().saturating_sub(1).to_string().len();
        let e_str = chap.sl.e.to_string();
        let mut names = Vec::with_capacity(chap.files.len());
        let mut needs_delay = false;
        for (i, file) in chap.files.iter().enumerate() {
            let file_safe = sanitize(file);
            let fname = format!("{:0width$}_{}", i, file_safe, width = width);
            let dst = chapter_dir.join(&fname);
            names.push(fname);

            if dst.exists() {
                bar.inc(1);
                continue;
            }
            // Space out consecutive downloads; no delay before the first one
            // or after the last one.
            if needs_delay {
                thread::sleep(rand::rng().random_range(self.delay / 2..=self.delay * 3 / 2));
            }
            // Built here rather than above the skip: a chapter already complete
            // on disk has no reason to fail over a page URL it never requests.
            let url = self.image_url(&chap.path, file)?;
            let mut resp = self
                .client
                .get(url)
                .header("accept", "image/webp,image/apng,image/*,*/*;q=0.8")
                .header("priority", "u=4")
                .header("referer", chapter_url.as_str())
                .header("sec-fetch-dest", "image")
                .header("sec-fetch-mode", "no-cors")
                .header("sec-fetch-site", "cross-site")
                .query(&[("e", &e_str), ("m", &chap.sl.m)])
                .send()?
                .error_for_status()?;

            // `None` unless the response carries an exact length: a chunked
            // transfer has none, and neither does a body reqwest decoded for us
            // — the client advertises gzip/deflate/br, and a decoded body
            // reports no exact size. So the length check below is a second line
            // of defence rather than the only one: wherever the framing says how
            // much is coming, the transport itself errors on a connection that
            // closes mid-body, which is why `test_download_incomplete_file`
            // accepts either message.
            //
            // What neither covers is a response framed by neither — no
            // `Content-Length`, not chunked, the body ending when the
            // connection does. There EOF *is* the terminator, so `io::copy`
            // returns `Ok` on a short body and the length check has no length
            // to check against: a truncated image would be sealed into the
            // .cbz. A decoded body still errors inside the decoder; an
            // undecoded one has nothing left to catch it.
            let content_length = resp.content_length();
            // Both checks belong inside the closure: returning `Ok` is what
            // renames the file into place, and a truncated or non-image page
            // under the final name would be skipped as finished by every later
            // run — there is no second chance to notice it.
            write_atomic(&dst, |out| {
                // Sniff the signature before writing anything. `error_for_status`
                // only rules out an error *status*; an anti-hotlink page served
                // as 200 would otherwise be sealed into the .cbz as a page.
                let mut head = Vec::with_capacity(IMAGE_HEAD_LEN);
                resp.by_ref()
                    .take(IMAGE_HEAD_LEN as u64)
                    .read_to_end(&mut head)?;
                if !looks_like_image(&head) {
                    return Err(AppError::parsing(format!(
                        "Page {} ({}) is not an image: response starts with {:02x?}",
                        i + 1,
                        file,
                        head
                    )));
                }
                out.write_all(&head)?;
                let bytes_written = head.len() as u64 + io::copy(&mut resp, out)?;
                if let Some(expected) = content_length
                    && bytes_written != expected
                {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("Incomplete download: expected {} bytes, got {}", expected, bytes_written),
                    )
                    .into());
                }
                Ok(())
            })?;
            bar.inc(1);
            needs_delay = true;
        }
        Ok(names)
    }

    /// Pack exactly the pages `download_images` reported, in the order it
    /// reported them.
    ///
    /// The archive contents must come from that list and not from whatever
    /// `chapter_dir` happens to hold: the zero padding of a page's file name is
    /// derived from the chapter's total page count, so a chapter that gained
    /// pages since an interrupted run leaves stale files behind under a
    /// narrower width (`0_a.webp` next to a new `00_a.webp`). Reading the
    /// directory and sorting would fold those duplicates in — and sort them
    /// into the wrong places, since `'0' < '_'` puts `0_a.webp` after
    /// `09_a.webp` — producing a scrambled .cbz that is then cached forever.
    fn compress_chapter(chapter_dir: &Path, file_names: &[String], zip_path: &Path) -> Result<()> {
        write_atomic(zip_path, |zip_file| {
            let mut zip = ZipWriter::new(zip_file);
            let options = FileOptions::default().compression_method(CompressionMethod::Stored);

            for name in file_names {
                zip.start_file(name.as_str(), options)?;
                let mut file = fs::File::open(chapter_dir.join(name))?;
                io::copy(&mut file, &mut zip)?;
            }

            // Finish explicitly rather than leaving it to `ZipWriter`'s `Drop`,
            // which has nowhere to report a failure to write out the central
            // directory. The handle itself is owned — and closed — by
            // `write_atomic`.
            zip.finish()?;
            Ok(())
        })?;
        // The .cbz is already in place; failing to clean up the now-redundant
        // image directory must not report the chapter as failed. Warn instead.
        if let Err(e) = fs::remove_dir_all(chapter_dir) {
            eprintln!(
                "Warning: failed to remove temporary directory {}: {}",
                chapter_dir.display(),
                e
            );
        }
        Ok(())
    }

    fn download_chapter(&self, index: usize) -> Result<bool> {
        let Chapter { name, href, .. } = &self.chapters[index];
        let chap_safe = sanitize(name);
        // Chapter names are unique across the whole comic on manhuagui (the
        // same name never appears in two groups), so `group` is intentionally
        // not part of the file name and name collisions are not a concern.
        let zip_path = self
            .book_dir
            .join(format!("{}_{}.cbz", self.book_safe, chap_safe));
        if zip_path.exists() {
            println!("{} already exists, skipping.", zip_path.display());
            return Ok(false);
        }
        let chapter_url = resolve_url(href)?;
        let html = fetch_html(&self.client, &chapter_url, &HOST_URL)?;
        let chap = parse_chapter_html(&html)?;
        let chapter_dir = self.book_dir.join(&chap_safe);
        fs::create_dir_all(&chapter_dir)?;
        let bar = ProgressBar::new(chap.files.len() as u64);
        bar.set_style(BAR_STYLE.clone());
        bar.set_message(name.clone());

        // Release the bar's draw state before anything else writes, on both
        // ways out. indicatif draws to stderr, so anything written while the
        // bar still owns its line lands on top of it — `compress_chapter` warns
        // there when it cannot remove the page directory, and the caller prints
        // the error from the other path on the same stream. The bar has reached
        // its final position by this point either way, so there is nothing left
        // for it to draw.
        let names = self
            .download_images(&chap, &chapter_dir, &bar, &chapter_url)
            .inspect_err(|_| bar.abandon())?;
        bar.finish();
        Self::compress_chapter(&chapter_dir, &names, &zip_path)?;
        Ok(true)
    }
}

/// Print `prompt`, then read one line and return it trimmed.
/// Errors with `UnexpectedEof` if the input stream is closed.
fn prompt_line<R: io::BufRead>(reader: &mut R, prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    if reader.read_line(&mut input)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stdin closed while waiting for input",
        )
        .into());
    }
    Ok(input.trim().to_string())
}

/// Prompt until `parse` accepts a line. A rejected line prints `error` and
/// re-prompts; only a closed input stream ends the loop, as an error.
///
/// A blank line re-prompts without the error. It is a stray Enter rather than
/// an answer — most often the one typed after the search pager's single-key
/// prompt, which returns on the key alone and has left raw mode by the time
/// the Enter arrives. Neither caller accepts an empty line anyway, so this
/// changes only what is printed.
fn prompt_until_valid<R: io::BufRead, T>(
    reader: &mut R,
    prompt: &str,
    error: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T> {
    loop {
        let input = prompt_line(reader, prompt)?;
        if input.is_empty() {
            continue;
        }
        if let Some(value) = parse(&input) {
            return Ok(value);
        }
        eprintln!("{error}");
    }
}

fn prompt_for_comic_selection<R: io::BufRead>(reader: &mut R, comics_count: usize) -> Result<usize> {
    prompt_until_valid(
        reader,
        "Select a comic (enter number): ",
        &format!(
            "Invalid selection. Please enter a number between 1 and {}.",
            comics_count
        ),
        |input| match input.parse::<usize>() {
            Ok(n) if (1..=comics_count).contains(&n) => Some(n - 1),
            _ => None,
        },
    )
}

/// Parse a 1-based chapter selection like "1-3,5" into sorted, deduped
/// 0-based indices. Each range's bounds are validated before it is expanded,
/// so a typo like "1-999999999" is rejected up front instead of allocating
/// billions of entries. Returns `None` on any syntax or bounds error; the
/// whole input is rejected rather than silently dropping the bad part.
fn parse_chapter_selection(input: &str, chapters_count: usize) -> Option<Vec<usize>> {
    let mut indices: Vec<usize> = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        let (start, end) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse().ok()?, b.trim().parse::<usize>().ok()?),
            None => {
                let n = part.parse().ok()?;
                (n, n)
            }
        };
        if start == 0 || start > end || end > chapters_count {
            return None;
        }
        indices.extend(start - 1..end);
    }
    indices.sort_unstable();
    indices.dedup();
    Some(indices)
}

fn prompt_for_chapters<R: io::BufRead>(reader: &mut R, chapters_count: usize) -> Result<Vec<usize>> {
    prompt_until_valid(
        reader,
        "Select chapters (e.g. 1-3,5): ",
        &format!(
            "Invalid selection. Please enter numbers between 1 and {} (e.g. 1-3,5).",
            chapters_count
        ),
        |input| parse_chapter_selection(input, chapters_count),
    )
}

/// Search for `keyword`, page through the results interactively, and let the
/// user pick a comic. Returns the selected comic's ID.
fn interactive_search<R: io::BufRead>(
    client: &Client,
    reader: &mut R,
    keyword: &str,
) -> Result<usize> {
    let mut all_results: Vec<SearchResult> = Vec::new();
    // The page the current href was read off: both the base it resolves
    // against and the `referer` the fetch sends. Seeded with the site root for
    // the first href, which this function writes itself as a site-absolute
    // path and so resolves the same way against either.
    let mut from_page = HOST_URL.clone();
    let mut next_href = Some(format!("/s/{}.html", urlencoding::encode(keyword)));

    println!("Search results for '{}':", keyword);

    while let Some(href) = next_href {
        // Resolving the href and fetching what it points at are one fallible
        // step, so that both ways of failing to reach a page — a connection
        // that drops on the way to page 3, or an href the pager wrote that does
        // not parse — meet the one policy below instead of each growing its
        // own. An href that parses to something other than a web page, such as
        // `javascript:;`, never gets this far: `parse_search_results` drops it
        // before the user is asked to press SPACE for it.
        //
        // The base is the page the href was read off, not the site root. The
        // pager writes site-absolute hrefs today, for which the two agree; a
        // document-relative one (`金田一_p2.html` rather than
        // `/s/金田一_p2.html`) lands under `/s/` only against the page it came
        // from. Getting that wrong would be quiet, too: the 404 it resolves to
        // reaches the warning below and reads there as an early end of results.
        let fetched = join_url(&from_page, &href).and_then(|url| {
            let html = fetch_html(client, &url, &from_page)?;
            Ok((url, html))
        });
        let (url, html) = match fetched {
            Ok(page) => page,
            // Results collected so far have already been printed and numbered,
            // and the user can still pick one of them: discarding them over a
            // page they have not asked for yet would mean running the whole
            // search again. Stop paging and let the selection prompt run.
            // The first page is the exception — with nothing to fall back on,
            // its failure stays the error the run ends with.
            Err(e) if !all_results.is_empty() => {
                eprintln!(
                    "Warning: could not load more results ({e}); \
                     continuing with the {} found so far.",
                    all_results.len()
                );
                break;
            }
            Err(e) => return Err(e),
        };
        from_page = url;

        let (page_results, maybe_next) = parse_search_results(&html);
        let offset = all_results.len();
        for (i, r) in page_results.iter().enumerate() {
            println!("{}. {}", offset + i + 1, r.title);
        }
        // A page that parsed to no results ends the paging, whatever its pager
        // offers: a genuine "no hits" page has nothing to page through, and if
        // the result selector has instead gone stale against a redesign, every
        // page parses to nothing — without this the pager would keep asking the
        // user to press SPACE for another screen of nothing. Breaking keeps
        // whatever earlier pages did parse; the check below turns a search that
        // parsed nothing at all into `NoSearchResults`.
        if page_results.is_empty() {
            break;
        }
        all_results.extend(page_results);

        // Only the href is kept here; it is resolved at the top of the next
        // iteration, where an href that does not parse is handled the same way
        // as a page that cannot be reached.
        //
        // The prompt stays inside the `Some` arm: a page with no pager link
        // must not ask the user whether to fetch one.
        next_href = match maybe_next {
            Some(next) => prompt_for_next_page()?.then_some(next),
            None => None,
        };
    }

    if all_results.is_empty() {
        return Err(AppError::NoSearchResults(keyword.to_string()));
    }

    let selected = prompt_for_comic_selection(reader, all_results.len())?;
    Ok(all_results[selected].comic_id)
}

/// `main` deliberately does not return `Result`: the `Termination` impl for
/// `Result<T, E>` reports the error with `Debug`, which would print
/// `NoSearchResults("金田一")` instead of the `#[error(...)]` text every
/// `AppError` variant carries. Report `Display` here and hand back a plain
/// exit code — this is the only place a user-facing error is printed.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let client = build_client()?;
    let mut stdin = io::stdin().lock();

    let id = if let Some(ref search_keyword) = args.search {
        interactive_search(&client, &mut stdin, search_keyword)?
    } else {
        let url = args
            .url
            .as_deref()
            .expect("clap's required_unless_present guarantees url when search is absent");
        parse_id(url).ok_or(AppError::InvalidUrl)?
    };

    let comic = Comic::new(id, client, &args)?;
    println!("Title: {}", comic.title);
    let mut last_group = "";
    for (i, chapter) in comic.chapters.iter().enumerate() {
        if chapter.group != last_group {
            println!("{}:", chapter.group);
            last_group = &chapter.group;
        }
        println!("  {}: {}", i + 1, chapter.name);
    }

    let indices = prompt_for_chapters(&mut stdin, comic.chapters.len())?;

    for (k, &idx) in indices.iter().enumerate() {
        // Whether to pause before the next chapter: after an actual download,
        // or after an error (to avoid hammering the server on a connection
        // issue). A skipped, already-present chapter needs no pause.
        let should_pause = match comic.download_chapter(idx) {
            Ok(downloaded) => downloaded,
            Err(e) => {
                // Named as well as numbered: a selection like "1-3,50" reports
                // failures out of order and far apart, and the number alone
                // means scrolling back to the listing to find out what broke.
                eprintln!(
                    "Failed to download chapter {} ({}): {}",
                    idx + 1,
                    comic.chapters[idx].name,
                    e
                );
                true
            }
        };
        if should_pause && k + 1 < indices.len() {
            thread::sleep(Duration::from_secs(5));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
