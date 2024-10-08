use anyhow::{bail, Context};
use bytes::{Buf, Bytes};
use clap::builder::{StringValueParser, TypedValueParser, ValueParserFactory};
use clap::{Arg, Command, Parser};
use console::Term;
use futures_util::StreamExt;
use indicatif::{HumanBytes, ProgressBar, ProgressDrawTarget, ProgressStyle};
use lazy_static::lazy_static;
use regex::Regex;
use reqwest::{Client, Url};
use std::ffi::OsStr;
use std::io::Write;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, timeout};

lazy_static! {
    static ref PROGRESS_STYLE: ProgressStyle = ProgressStyle::with_template(
        "{percent}% <{bar:20.cyan/blue}> {bytes}/{total_bytes} ({binary_bytes_per_sec}) {wide_msg} {elapsed}/{duration} ({eta} remaining)"
    )
    .unwrap()
    .progress_chars("=+-");
    static ref HEADER_DELIMITER: Regex = Regex::new(" ?: ?").unwrap();
}

/// Stubborn-downloader-rs is a re-creation of my stubborn-downloader program in
/// rust. Stubborn-downloader-rs attempts to re-create the important curl
/// options.
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Url to download from.
    #[clap(value_parser)]
    url: Url,

    /// Optional headers to send with every request.
    #[clap(short = 'H', long = "header")]
    headers: Vec<Header>,

    /// File to output to.
    #[clap(short, long, value_parser)]
    output: PathBuf,

    /// Number of seconds to wait after every error.
    #[clap(long, value_parser, default_value_t = 10)]
    error_delay: u32,

    /// The amount of time to wait before re-opening a stalled connection.
    #[clap(long, value_parser, default_value_t = 60)]
    connection_timeout: u32,

    /// Use an existing output as if it were an incomplete download and add to
    /// it.
    #[clap(long)]
    use_existing: bool,
}

/// Struct for holding header key-value pairs.
#[derive(Debug, Clone)]
struct Header {
    key: String,
    value: String,
}

impl ValueParserFactory for Header {
    type Parser = SimpleHeaderValueParser;

    fn value_parser() -> Self::Parser {
        SimpleHeaderValueParser
    }
}

#[derive(Debug, Copy, Clone)]
struct SimpleHeaderValueParser;
impl TypedValueParser for SimpleHeaderValueParser {
    type Value = Header;

    fn parse_ref(
        &self,
        cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let str = StringValueParser::new().parse_ref(cmd, arg, value)?;
        let (key, value) = str.split_once(HEADER_DELIMITER.deref()).ok_or_else(|| {
            clap::Error::raw(clap::error::ErrorKind::ValueValidation, "Header arguments must contain a colon (':') to separate header key from header value.").with_cmd(cmd)
        })?;

        Ok(Header {
            key: key.to_string(),
            value: value.to_string(),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::new();

    let term = Term::stderr();

    let args: Args = Args::parse();

    writeln!(&term, "################").unwrap();
    writeln!(&term, "Download Info:").unwrap();
    writeln!(&term, "URL: {}", &args.url).unwrap();

    writeln!(&term, "Headers:").unwrap();
    for header in args.headers.iter() {
        writeln!(&term, "    {}: {}", header.key, header.value).unwrap();
    }

    writeln!(&term, "Output file: {}", args.output.to_string_lossy()).unwrap();
    writeln!(&term, "Using existing file: {}", args.use_existing).unwrap();
    writeln!(&term, "################").unwrap();

    {
        let scheme = args.url.scheme();
        if scheme != "http" && scheme != "https" {
            bail!("Download urls must be in either HTTP or HTTPS");
        }
    }
    let headers: Vec<_> = args
        .headers
        .iter()
        .filter(|header| {
            if header.key.eq_ignore_ascii_case("range") {
                writeln!(&term, "# Encountered redundant 'Range' header. Ignoring...").unwrap();
                false
            } else {
                true
            }
        })
        .collect();

    let mut output_open = OpenOptions::new();

    if args.use_existing {
        output_open.append(true);
    } else {
        output_open.create_new(true);
    }

    let mut output = output_open.write(true).open(&args.output).await?;

    let connection_timeout = Duration::from_secs(args.connection_timeout as u64);

    let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::term(term.clone(), 8))
        .with_style(ProgressStyle::default_spinner())
        .with_message("Starting download...");

    let should_tick = Arc::new(AtomicBool::new(true));
    let should_tick_2 = should_tick.clone();
    let bar_2 = bar.clone();
    tokio::spawn(async move {
        while should_tick_2.load(Ordering::Relaxed) {
            bar_2.tick();
            sleep(Duration::from_millis(125)).await;
        }
    });

    let mut full_length = None;

    let mut offset = if args.use_existing {
        output.metadata().await?.len()
    } else {
        0u64
    };

    let mut incomplete = true;

    while incomplete {
        match do_download(
            &client,
            args.url.clone(),
            &headers,
            &mut output,
            connection_timeout,
            term.clone(),
            &bar,
            &mut full_length,
            &mut offset,
            &mut incomplete,
        )
        .await
        {
            Ok(_) => {}
            Err(err) => {
                writeln!(&term, "{:#}", err).unwrap();
                writeln!(&term, "Download error. Retrying...").unwrap();
            }
        }

        if incomplete && args.error_delay > 0 {
            writeln!(&term, "Sleeping for {} seconds", args.error_delay).unwrap();
            sleep(Duration::from_secs(args.error_delay as u64)).await;
        }
    }

    bar.finish();
    should_tick.store(false, Ordering::Relaxed);
    Ok(())
}

async fn do_download(
    client: &Client,
    url: Url,
    headers: &[&Header],
    output: &mut File,
    connection_timeout: Duration,
    term: Term,
    bar: &ProgressBar,
    full_length: &mut Option<u64>,
    offset: &mut u64,
    incomplete: &mut bool,
) -> anyhow::Result<()> {
    bar.set_message("Connecting...");
    bar.set_style(ProgressStyle::default_spinner());

    let mut builder = client.get(url);

    if *offset > 0u64 {
        builder = builder.header("range", format!("bytes={}-", offset));
    }

    for header in headers {
        builder = builder.header(&header.key, &header.value);
    }

    let res = timeout(connection_timeout, builder.send())
        .await
        .context("Connection timeout")?
        .context("Error connecting to server")?;

    if res.status().is_client_error() || res.status().is_server_error() {
        writeln!(&term, "!!!!!!!!!!!!!!!!").unwrap();
        writeln!(&term, "Server gave bad response code: {}", res.status()).unwrap();
        writeln!(&term, "!!!!!!!!!!!!!!!!").unwrap();
        bar.set_message("Error.");
        *incomplete = false;
        return Ok(());
    }

    let length = res.content_length();
    let mut downloaded = 0u64;
    if full_length.is_none() {
        *full_length = length.map(|len| *offset + len);
    }

    writeln!(&term, "================").unwrap();
    writeln!(&term, "Starting Download...").unwrap();
    if let Some(full_length) = full_length {
        writeln!(&term, "Downloading: {}", HumanBytes(*full_length)).unwrap();
    } else {
        writeln!(&term, "Content-Length not specified!").unwrap();
    }
    if let Some(length) = length {
        writeln!(&term, "Remaining  : {}", HumanBytes(length)).unwrap();
    }
    writeln!(&term, "Offset     : {}", HumanBytes(*offset)).unwrap();

    bar.set_message("Downloading...");

    if let Some(content_length) = full_length {
        bar.set_style(PROGRESS_STYLE.clone());
        bar.set_length(*content_length);
        bar.set_position(*offset);
    }

    let mut stream = res.bytes_stream();

    while let Some(item) = timeout(connection_timeout, stream.next())
        .await
        .context("Chunk download timeout")?
    {
        let chunk: Bytes = item.context("Error downloading byte chunk")?;

        output
            .write_all(chunk.chunk())
            .await
            .context("Error writing to file")?;

        let len = chunk.len() as u64;
        *offset += len;
        downloaded += len;
        bar.set_position(*offset);
    }

    if length.is_some_and(|length| downloaded < length) {
        bail!("Incomplete download");
    } else {
        writeln!(&term, "Download complete!").unwrap();
        writeln!(&term, "Downloaded: {}", HumanBytes(*offset)).unwrap();
        *incomplete = false;
    }

    Ok(())
}
