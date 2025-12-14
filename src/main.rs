/*
    Project: RetypeOS Collect CLI
    Context: High-performance file traversal and filtering tool.

    Architecture:
    1. CLI Parsing (Clap)
    2. Configuration Builder (Domain Logic)
    3. Traversal Engine (ignore crate wrapper)
    4. Pipeline Processor (Filter -> Stream -> Output)
*/

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use memchr::memchr;
use regex::Regex;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// =============================================================================
// MODULE: CLI DEFINITIONS
// =============================================================================

#[derive(Parser, Debug)]
#[command(
    name = "collect",
    author = "RetypeOS",
    version = "1.1.0",
    about = "Optimized file collector and filtering tool.",
    long_about = "Traverses directory trees respecting gitignore, applies filters, and optionally captures content."
)]
struct Cli {
    /// Base directory to start searching from.
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Include file content in the output.
    #[arg(long)]
    content: bool,

    /// Filter by file extensions (comma separated, e.g., rs,toml).
    #[arg(long, value_delimiter = ',', group = "extension_filter")]
    extension: Option<Vec<String>>,

    /// Exclude by file extensions (comma separated, e.g., py,js).
    /// Cannot be used with --extension.
    #[arg(long, value_delimiter = ',', group = "extension_filter")]
    no_extension: Option<Vec<String>>,

    /// Regex pattern to apply.
    #[arg(long)]
    regex: Option<String>,

    /// Scope of the regex/pattern application.
    #[arg(long, value_enum, default_value_t = Scope::Name)]
    scope: Scope,

    /// Invert regex filter.
    #[arg(long)]
    regex_inv: bool,

    // TODO Features
    #[arg(long)]
    pattern: Option<String>,
    #[arg(long)]
    metadata: Option<String>,

    /// Maximum search depth (0 = base only).
    #[arg(long)]
    depth: Option<usize>,

    /// Explicitly exclude files/folders patterns (e.g., "target", "*.log").
    #[arg(long, value_delimiter = ',')]
    exclude: Option<Vec<String>>,

    /// Disable default excludes (gitignore, hidden, etc).
    #[arg(long)]
    no_default_excludes: bool,

    /// Follow symbolic links.
    #[arg(long)]
    follow_symlinks: bool,

    /// Include hidden files.
    #[arg(long)]
    include_hidden: bool,

    /// Output to a file instead of stdout.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Max bytes to read per file when using --content.
    #[arg(long)]
    max_bytes: Option<u64>,

    /// Use absolute paths in output header.
    #[arg(long)]
    absolute: bool,

    /// Reduce warnings and metadata info.
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Show usage guide.
    #[arg(long)]
    guide: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Debug)]
enum Scope {
    Name,
    Path,
}

// =============================================================================
// MODULE: CORE LOGIC & CONFIG
// =============================================================================

/// Runtime configuration optimized for lookup speed.
/// Now includes all fields required by the walker and processor.
struct AppConfig {
    // Filters
    extensions: Option<Vec<String>>,
    extension_inv: bool,
    regex: Option<Regex>,
    regex_inv: bool,
    scope: Scope,

    // Walker Config
    base_path: PathBuf,
    depth: Option<usize>,
    exclude: Option<Vec<String>>,
    no_default_excludes: bool,
    include_hidden: bool,
    follow_symlinks: bool,

    // Output Config
    output: Option<PathBuf>,
    absolute_path: bool,
    max_bytes: Option<u64>,
    read_content: bool,
    quiet: bool,
}

impl AppConfig {
    fn from_cli(cli: Cli) -> Result<Self> {
        let regex = if let Some(re_str) = cli.regex {
            Some(Regex::new(&re_str).context("Invalid Regex format")?)
        } else {
            None
        };

        // Determine if we are allowing or excluding extensions
        // Since they are in a Clap group, only one (or none) can be present.
        let (raw_extensions, extension_inv) = if let Some(exts) = cli.extension {
            (Some(exts), false) // Whitelist mode
        } else if let Some(exts) = cli.no_extension {
            (Some(exts), true) // Blacklist mode
        } else {
            (None, false)
        };

        // Normalize extensions to lowercase for case-insensitive comparison
        let extensions = raw_extensions.map(|exts| {
            exts.into_iter()
                .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                .collect()
        });

        Ok(Self {
            extensions,
            extension_inv,
            regex,
            regex_inv: cli.regex_inv,
            scope: cli.scope,
            base_path: cli.path,
            depth: cli.depth,
            exclude: cli.exclude,
            no_default_excludes: cli.no_default_excludes,
            include_hidden: cli.include_hidden,
            follow_symlinks: cli.follow_symlinks,
            output: cli.output,
            absolute_path: cli.absolute,
            max_bytes: cli.max_bytes,
            read_content: cli.content,
            quiet: cli.quiet,
        })
    }
}

// =============================================================================
// MODULE: FILTER PIPELINE
// =============================================================================

/// Evaluates if a path matches the criteria.
/// This is the "hot path" of the application, keep it allocation-free if possible.
fn should_process(path: &Path, config: &AppConfig, is_dir: bool) -> bool {
    // 1. Extension Filter (O(1) lookup effectively for small lists)
    if !is_dir && let Some(exts) = &config.extensions {
        let file_ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();

        let found = exts.contains(&file_ext);
        if found == config.extension_inv {
            return false;
        }
    }

    // 2. Regex Filter (Expensive, do it last)
    if let Some(re) = &config.regex {
        let text_to_match = match config.scope {
            Scope::Name => path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            Scope::Path => path.to_str().unwrap_or(""),
        };

        let found = re.is_match(text_to_match);
        if found == config.regex_inv {
            return false;
        }
    }

    true
}

// =============================================================================
// MODULE: I/O PROCESSOR (Optimized)
// =============================================================================

/// Handles file reading and writing with buffering.
/// Returns io::Result to allow easier BrokenPipe handling in main.
fn process_file(
    path: &Path,
    config: &AppConfig,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
) -> io::Result<()> {
    // 1. Path Formatting
    let path_display = if config.absolute_path {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.strip_prefix(&config.base_path)
            .unwrap_or(path)
            .to_path_buf()
    };

    // 2. Write Header
    if config.read_content {
        writeln!(writer, "=== {} ===", path_display.display())?;
    } else {
        writeln!(writer, "{}", path_display.display())?;
    }

    // 3. Content Streaming (The optimization core)
    if config.read_content {
        stream_file_content(path, writer, config.max_bytes)?;
    }

    Ok(())
}

/// Reads file with binary detection and streams to output.
/// Uses a 8KB buffer to detect binary files (null bytes) and respects max_bytes immediately.
fn stream_file_content(
    path: &Path,
    writer: &mut BufWriter<Box<dyn Write + Send>>,
    max_bytes: Option<u64>,
) -> io::Result<()> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            writeln!(writer, "\n<Error opening file: {}>\n", e)?;
            return Ok(());
        }
    };

    let mut reader = BufReader::new(file);
    // 8KB buffer for heuristic binary check
    let mut buffer = [0u8; 8192];

    // Read first chunk
    let n = reader.read(&mut buffer)?;

    if n == 0 {
        writeln!(writer, "\n<Empty File>\n")?;
        return Ok(());
    }

    // SIMD Optimized search for null byte to detect binary
    if memchr(0, buffer.get(..n).expect("Failed to read file")).is_some() {
        writeln!(writer, "\n<Binary content suppressed>\n")?;
        return Ok(());
    }

    // Determine the absolute limit logic
    let limit = max_bytes.unwrap_or(u64::MAX);

    // Calculate how many bytes from the INITIAL buffer we are allowed to write.
    // If limit is 100 but we read 8192, we only write 100.
    // If limit is 1GB and we read 8192, we write 8192.
    let bytes_to_write_from_buffer = usize::try_from(std::cmp::min(n as u64, limit))
        .expect("Unexpected error trying to convert limit to usize.");

    writer.write_all(b"\n")?;
    writer.write_all(
        buffer
            .get(..bytes_to_write_from_buffer)
            .expect("Failed to read file"),
    )?;

    // If we haven't reached the limit yet AND there might be more file content
    if limit > bytes_to_write_from_buffer as u64 {
        let remaining_allowance = limit - bytes_to_write_from_buffer as u64;

        // Use 'take' to wrap the reader, ensuring we never cross the boundary
        // during the streaming copy.
        let mut limited_reader = reader.take(remaining_allowance);

        // Zero-copy stream (kernel space copy where supported)
        io::copy(&mut limited_reader, writer)?;
    }

    // Optional: Indicate if truncated?
    // Usually CLI tools just stop, but for debugging valid to know.
    // We stick to simple output for now.

    writer.write_all(b"\n\n")?;

    Ok(())
}

// =============================================================================
// MODULE: GUIDE & HELPERS
// =============================================================================

fn print_guide() {
    println!(
        r#"
    RETYPEOS COLLECT - USER GUIDE
    =============================

    FILTERS:
      --extension rs,toml    : Only allow .rs and .toml files.
      --no-extension py,js   : Allow everything EXCEPT .py and .js files.
      --regex "Test.*"       : Allow files matching regex.
      --scope path           : Regex applies to full relative path.
      
    (Note: --extension and --no-extension are mutually exclusive)

    CONTENT & LIMITS:
      --content              : Read and print file content.
      --max-bytes 1000       : Truncate reading after 1000 bytes.
      --depth 2              : Only go 2 folders deep.
      --output file.txt      : Save result to file.

    EXCLUDES:
      Default: Ignores .git, target/, node_modules/ and hidden files.
      --no-default-excludes  : Scan everything.
      --include-hidden       : Include hidden files.
      --exclude "log,tmp"    : Add custom exclusion patterns.

    PERFORMANCE TIPS:
      - Use --output for large datasets.
      - Binary files are automatically detected and skipped.
    "#
    );
}

// =============================================================================
// MAIN ENTRY POINT
// =============================================================================

fn main() -> Result<()> {
    // Initialize CLI
    let cli = Cli::parse();

    if cli.guide {
        print_guide();
        return Ok(());
    }

    if cli.pattern.is_some() {
        eprintln!("Info: --pattern is currently in TODO status. Ignoring.");
    }
    if cli.metadata.is_some() {
        eprintln!("Info: --metadata is currently in TODO status. Ignoring.");
    }

    // Build Configuration
    let config = Arc::new(AppConfig::from_cli(cli)?);

    // Setup Output Strategy
    let raw_writer: Box<dyn Write + Send> = match &config.output {
        Some(path) => Box::new(File::create(path).context("Failed to create output file")?),
        None => Box::new(io::stdout()),
    };

    // Large buffer (64KB) for fewer syscalls
    let writer = Arc::new(Mutex::new(BufWriter::with_capacity(64 * 1024, raw_writer)));

    // Setup Walker (The Traversal Engine)
    let mut builder = WalkBuilder::new(&config.base_path);
    builder
        .standard_filters(!config.no_default_excludes)
        .hidden(!config.include_hidden)
        .follow_links(config.follow_symlinks)
        .max_depth(config.depth)
        .threads(1); // Force single thread for deterministic output order

    if let Some(excludes) = &config.exclude {
        let mut override_builder = OverrideBuilder::new(&config.base_path);
        for exc in excludes {
            // ! negates the ignore, meaning "include", but in .gitignore syntax
            // ! matches mean exclude if using ignore builder carefully.
            // But here standard convention for cli override is just passed patterns.
            // Let's assume standard gitignore logic: "foo" ignores foo.
            override_builder.add(&format!("!{}", exc))?;
        }
        builder.overrides(override_builder.build()?);
    }

    let walker = builder.build();
    let start = Instant::now();
    let mut count = 0;

    // Execution
    for result in walker {
        match result {
            Ok(entry) => {
                let path = entry.path();

                // Skip root itself
                if entry.depth() == 0 {
                    continue;
                }

                let is_dir = entry.file_type().map(|f| f.is_dir()).unwrap_or(false);

                // Apply Filters
                if should_process(path, &config, is_dir) && !is_dir {
                    let mut w_guard = writer
                        .lock()
                        .expect("Unexpected error trying lock writter.");

                    // Handle IO errors directly
                    if let Err(e) = process_file(path, &config, &mut w_guard) {
                        // Gracefully exit on BrokenPipe (e.g., piped to `head`)
                        if e.kind() == io::ErrorKind::BrokenPipe {
                            return Ok(());
                        }
                        if !config.quiet {
                            eprintln!("Error processing {}: {}", path.display(), e);
                        }
                    }
                    count += 1;
                }
            }
            Err(err) => {
                if !config.quiet {
                    eprintln!("Traversal Error: {}", err);
                }
            }
        }
    }

    // Flush remaining buffer
    {
        let mut w = writer
            .lock()
            .expect("Unexpected error trying lock writter.");
        if let Err(e) = w.flush()
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(e.into());
        }
    }

    if !config.quiet && config.output.is_none() {
        eprintln!("Done. Processed {} files in {:.2?}", count, start.elapsed());
    }

    Ok(())
}
