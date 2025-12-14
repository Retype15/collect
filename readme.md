# Collect CLI

## **High-Performance File Traversal & Aggregation Tool**

`collect` is a blazing fast, memory-efficient command-line tool written in **Rust**. It is designed to traverse directory trees, respect ignore rules (like `.gitignore`), filter files with granular precision, and aggregate their content into a single stream or file.

It is particularly useful for:

- Preparing codebases for LLM (Large Language Model) context windows.
- Auditing file contents across large projects.
- Rapid file searching using advanced filters.

> **Current Version:** 1.0.0 (Stable)

---

## 🚀 Features & Philosophy

This tool adheres to the philosophy of **"Obsessive Optimization"**:

- **Zero-Copy Streaming**: Files are streamed directly from disk to output using a fixed-size buffer (8KB). No file is ever fully loaded into RAM, allowing processing of multi-gigabyte files with negligible memory footprint.
- **SIMD Binary Detection**: Uses `memchr` (AVX/SSE optimized) to scan the first chunk of every file. Binary files are detected and skipped instantly to prevent terminal corruption.
- **Smart Traversal**: Powered by the `ignore` crate (same engine as `ripgrep`), it respects `.gitignore`, `.ignore`, and global exclude files natively.
- **Buffered I/O**: Output is wrapped in a 64KB `BufWriter` to minimize syscalls during massive writes.
- **Robustness**: Handles broken pipes (e.g., `collect | head`) gracefully without panics.

---

## 📦 Installation

### Option 1: Pre-compiled Binaries (Recommended)

Download the latest binary for your operating system (Windows, macOS, or Linux) directly from our **Releases Page**.

1. Go to **[Releases](https://github.com/Retype15/collect/releases/latest)**.
2. Download the archive for your architecture.
3. Extract the binary and place it in your system `PATH`.

### Option 2: Build from Source

If you prefer to build it yourself, ensure you have Rust installed.

```bash
git clone https://github.com/Retype15/collect.git
cd collect
cargo build --release
```

The binary will be located at `./target/release/collect`.

---

## 🛠 Usage

```bash
collect [OPTIONS] --path <PATH>
```

### Core Options

| Flag | Description |
|------|-------------|
| `--path <PATH>` | Base directory to start searching (Default: `.`). |
| `--content` | Reads and outputs the file content. If omitted, only lists paths. |
| `--output <FILE>` | Writes output to a file (atomic, buffered) instead of stdout. |
| `--max-bytes <N>` | Truncates reading of each file after N bytes. |
| `--depth <N>` | Limits the directory traversal depth (0 = root only). |

### Filtering

| Flag | Description |
|------|-------------|
| `--extension <EXT>` | Comma-separated list of extensions (e.g., `rs,toml`). |
| `--no-extension` | Inverts extension filter (Allow everything EXCEPT listed). |
| `--regex <PATTERN>` | Applies a Regex pattern to the filename. |
| `--scope <name\|path>`| Applies Regex to filename or full relative path. Default: `name`. |
| `--regex-inv` | Inverts the Regex match. |
| `--exclude <LIST>` | Custom exclusions (e.g., `target,node_modules`). |

### Traversal Behavior

| Flag | Description |
|------|-------------|
| `--no-default-excludes` | Forces scanning of `.git`, hidden files, and ignored files. |
| `--include-hidden` | Includes hidden files (starting with `.`) in the search. |
| `--follow-symlinks` | Follows symbolic links to their targets. |
| `--absolute` | Outputs absolute paths in the headers. |

---

## 💡 Examples

### 1. Prepare a Rust Project for an LLM

Collects all source code and config files, ignoring lockfiles and binaries, truncated to 100KB per file.

```bash
collect --path . \
  --extension rs,toml,md \
  --content \
  --max-bytes 102400 \
  --output context.txt
```

### 2. Find Specific Configurations

Finds all files containing "config" in their path (not just name), excluding JSON files.

```bash
collect \
  --regex "config" \
  --scope path \
  --extension json \
  --extension-inv \
  --content
```

### 3. Quick Audit of a specific directory depth

Lists non-hidden files 2 levels deep.

```bash
collect --depth 2 --content
```

---

## 🏗 Technical Architecture

### The Pipeline

1. **Walk Engine**: Uses a single-threaded `WalkBuilder` (to ensure deterministic output order for "tree" visualization) that efficiently filters inodes based on file type and global ignore rules.
2. **Filter Logic**:
    - **Level 1 (Cheap)**: Boolean checks (Is Dir?) and Hash lookups (Extensions).
    - **Level 2 (Expensive)**: Regex compilation and matching.
3. **Content Processor**:
    - Opens file -> Creates `BufReader` -> Reads 8KB chunk.
    - **Heuristic Check**: Scans for `\0` (null byte) using SIMD.
    - **Streaming**: Writes the buffer to the Output `BufWriter` while strictly adhering to `--max-bytes`.
    - **Zero-Copy**: Uses `std::io::copy` (splice/sendfile) for the remainder of the file if limits allow.

### Error Handling

- **Broken Pipes**: If piped to tools like `head` or `less` which close the stream early, `collect` detects `io::ErrorKind::BrokenPipe` and exits cleanly with code 0.
- **Permission Denied**: Logs a warning to stderr (unless `--quiet` is set) and continues traversal.
