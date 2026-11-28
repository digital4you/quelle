//! PDF OCR fallback prioritizing permissive Apache-2.0 engines:
//!   1. LiteParse (`lit`) with bundled Apache-2.0 Tesseract (structured Markdown + tables,
//!      ~1.9 s / 222 MiB on 2 small CPU cores). `QUELLE_OCR_BACKEND=ocria-nano` remains
//!      strictly opt-in (restricted OpenRAIL-M license).
//!   2. Direct `pdftoppm` + `tesseract` CLI (Apache-2.0, ~75 MiB RSS, zero extra tools).
//!   3. Optional HTTP OCR sidecar (`QUELLE_OCR_URL`) for Apache-2.0 GPU VLMs such as
//!      IBM Granite-Docling-258M, LightOnOCR-2, or PaddleOCR-VL.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use reqwest::header;
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::Instant,
};
use url::Url;

use super::{
    DOCUMENT_SLOTS, MAX_PDF_MAX_PAGES, PDF_TEXT_BYTES, PageError, effective_pdf_max_pages,
};
use crate::read_body;

const OCR_TIMEOUT: Duration = Duration::from_secs(45);
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OcrMode {
    #[default]
    Auto,
    Liteparse,
    Tesseract,
    Sidecar,
    Off,
}

impl OcrMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "liteparse" | "ocria" | "ocria-nano" => Some(Self::Liteparse),
            "tesseract" => Some(Self::Tesseract),
            "sidecar" | "http" => Some(Self::Sidecar),
            "off" | "none" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LiteparseBackend {
    /// Apache-2.0 default: unrestricted commercial use, ~1.9 s / 222 MiB on 2 CPU cores.
    #[default]
    Tesseract,
    /// Opt-in only: LlamaIndex OCRia-nano-1 (restricted OpenRAIL-M: >$2M revenue/funding
    /// and competing-service restrictions).
    OcriaNano,
}

impl LiteparseBackend {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tesseract" => Some(Self::Tesseract),
            "ocria" | "ocria-nano" | "ocria-nano-1" => Some(Self::OcriaNano),
            _ => None,
        }
    }

    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Tesseract => "tesseract",
            Self::OcriaNano => "ocria-nano",
        }
    }

    pub fn retrieval_label(self) -> &'static str {
        match self {
            Self::Tesseract => "pdf-tesseract",
            Self::OcriaNano => "pdf-ocria",
        }
    }
}

static OCR_MODE: OnceLock<OcrMode> = OnceLock::new();
static LITEPARSE_BACKEND: OnceLock<LiteparseBackend> = OnceLock::new();
static LITEPARSE_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
static TESSERACT_CLI: OnceLock<bool> = OnceLock::new();
static OCR_SIDECAR_URL: OnceLock<Option<Url>> = OnceLock::new();

pub fn set_ocr_mode(mode: OcrMode) {
    let _ = OCR_MODE.set(mode);
}

pub fn ocr_mode() -> OcrMode {
    OCR_MODE.get().copied().unwrap_or(OcrMode::Auto)
}

pub fn set_liteparse_backend(backend: LiteparseBackend) {
    let _ = LITEPARSE_BACKEND.set(backend);
}

pub fn liteparse_backend() -> LiteparseBackend {
    LITEPARSE_BACKEND
        .get()
        .copied()
        .unwrap_or(LiteparseBackend::Tesseract)
}

pub fn set_ocr_sidecar(url: Option<Url>) {
    let _ = OCR_SIDECAR_URL.set(url);
}

pub(crate) fn ocr_sidecar() -> Option<&'static Url> {
    OCR_SIDECAR_URL.get().and_then(Option::as_ref)
}

/// Probes once at startup for LiteParse (`QUELLE_LITEPARSE_BIN` or `lit` on PATH).
pub fn detect_liteparse() -> bool {
    LITEPARSE_BIN
        .get_or_init(|| {
            let binary = std::env::var_os("QUELLE_LITEPARSE_BIN")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("PATH").and_then(|paths| {
                        std::env::split_paths(&paths)
                            .map(|dir| dir.join("lit"))
                            .find(|candidate| candidate.is_file())
                    })
                })?;
            let ok = std::process::Command::new(&binary)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            ok.then_some(binary)
        })
        .is_some()
}

/// Probes once at startup for Apache-2.0 `pdftoppm` + `tesseract` CLI tools.
pub fn detect_tesseract_cli() -> bool {
    *TESSERACT_CLI.get_or_init(|| {
        let has_pdftoppm = std::process::Command::new("pdftoppm")
            .arg("-v")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        let has_tesseract = std::process::Command::new("tesseract")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        has_pdftoppm && has_tesseract
    })
}

fn liteparse_bin() -> Option<&'static Path> {
    LITEPARSE_BIN.get().and_then(Option::as_deref)
}

fn tesseract_cli_enabled() -> bool {
    TESSERACT_CLI.get().copied().unwrap_or(false)
}

pub(crate) fn any_ocr_enabled() -> bool {
    match ocr_mode() {
        OcrMode::Off => false,
        OcrMode::Liteparse => liteparse_bin().is_some(),
        OcrMode::Tesseract => tesseract_cli_enabled(),
        OcrMode::Sidecar => ocr_sidecar().is_some(),
        OcrMode::Auto => {
            liteparse_bin().is_some() || tesseract_cli_enabled() || ocr_sidecar().is_some()
        }
    }
}

/// Builds the exact `lit parse` CLI argument list.
pub(crate) fn build_liteparse_args(backend: LiteparseBackend, max_pages: usize) -> Vec<String> {
    vec![
        "parse".to_owned(),
        "--format".to_owned(),
        "markdown".to_owned(),
        "--ocr-backend".to_owned(),
        backend.as_arg().to_owned(),
        "--image-mode".to_owned(),
        "off".to_owned(),
        "--max-pages".to_owned(),
        max_pages.clamp(1, MAX_PDF_MAX_PAGES).to_string(),
        "-q".to_owned(),
        "-".to_owned(),
    ]
}

/// Safe environment allowlist forwarded to OCR subprocesses so they locate
/// runtime models/tessdata without inheriting Quelle secrets (`QUELLE_TOKEN`,
/// API keys, etc.).
const SAFE_OCR_ENV: &[&str] = &[
    "HOME",
    "XDG_CACHE_HOME",
    "LITEPARSE_MODEL_DIR",
    "ORT_DYLIB_PATH",
    "TESSDATA_PREFIX",
    "PATH",
    "TMPDIR",
    "OMP_NUM_THREADS",
];

pub(crate) struct OcrOutput {
    pub markdown: String,
    pub retrieval: &'static str,
    pub pages_read: Option<usize>,
}

/// Runs OCR on a scanned / image-only PDF according to `QUELLE_OCR_MODE`.
pub(crate) async fn run_pdf_ocr(
    url: &Url,
    bytes: &[u8],
    max_pages: Option<usize>,
    deadline: Instant,
) -> Result<OcrOutput, PageError> {
    let mode = ocr_mode();
    if mode == OcrMode::Off {
        return Err(PageError::NoTextLayer);
    }

    let page_cap = effective_pdf_max_pages(max_pages);
    let backend = liteparse_backend();
    let has_next_fallback = tesseract_cli_enabled() || ocr_sidecar().is_some();

    if matches!(mode, OcrMode::Liteparse | OcrMode::Auto)
        && let Some(binary) = liteparse_bin()
    {
        match run_liteparse_binary(binary, backend, bytes, page_cap, deadline).await {
            Ok(markdown) => {
                tracing::info!(
                    stage = "pdf_ocr_liteparse",
                    backend = backend.as_arg(),
                    host = url.host_str(),
                    chars = markdown.len(),
                    ok = true
                );
                return Ok(OcrOutput {
                    markdown,
                    retrieval: backend.retrieval_label(),
                    pages_read: None,
                });
            }
            Err(err) if mode == OcrMode::Liteparse || !has_next_fallback => {
                tracing::info!(
                    stage = "pdf_ocr_liteparse",
                    backend = backend.as_arg(),
                    host = url.host_str(),
                    error = err.code(),
                    ok = false
                );
                return Err(err);
            }
            Err(err) => {
                tracing::info!(
                    stage = "pdf_ocr_liteparse_fallback",
                    backend = backend.as_arg(),
                    host = url.host_str(),
                    error = err.code()
                );
            }
        }
    }

    if matches!(mode, OcrMode::Tesseract | OcrMode::Auto) && tesseract_cli_enabled() {
        match run_tesseract_cli(bytes, page_cap, deadline).await {
            Ok((text, pages_read)) => {
                tracing::info!(
                    stage = "pdf_ocr_tesseract",
                    host = url.host_str(),
                    pages = pages_read,
                    chars = text.len(),
                    ok = true
                );
                return Ok(OcrOutput {
                    markdown: text,
                    retrieval: "pdf-tesseract",
                    pages_read: Some(pages_read),
                });
            }
            Err(err) if mode == OcrMode::Tesseract || ocr_sidecar().is_none() => {
                tracing::info!(
                    stage = "pdf_ocr_tesseract",
                    host = url.host_str(),
                    error = err.code(),
                    ok = false
                );
                return Err(err);
            }
            Err(err) => {
                tracing::info!(
                    stage = "pdf_ocr_tesseract_fallback",
                    host = url.host_str(),
                    error = err.code()
                );
            }
        }
    }

    if matches!(mode, OcrMode::Sidecar | OcrMode::Auto)
        && let Some(endpoint) = ocr_sidecar()
    {
        return run_http_ocr_sidecar(endpoint, url, bytes, deadline).await;
    }

    Err(PageError::NoTextLayer)
}

fn configure_safe_env(command: &mut Command) {
    command.env_clear();
    for key in SAFE_OCR_ENV {
        if let Some(val) = std::env::var_os(key) {
            command.env(key, val);
        }
    }
}

pub(crate) async fn run_liteparse_binary(
    binary: &Path,
    backend: LiteparseBackend,
    bytes: &[u8],
    max_pages: usize,
    deadline: Instant,
) -> Result<String, PageError> {
    let _slot = DOCUMENT_SLOTS
        .acquire()
        .await
        .map_err(|_| PageError::Unreadable)?;
    let step_deadline = deadline.min(Instant::now() + OCR_TIMEOUT);
    let args = build_liteparse_args(backend, max_pages);

    let mut command = Command::new(binary);
    configure_safe_env(&mut command);
    let mut child = command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| PageError::NoTextLayer)?;

    let mut stdin = child.stdin.take().ok_or(PageError::NoTextLayer)?;
    let mut stdout = child.stdout.take().ok_or(PageError::NoTextLayer)?;
    let write_bytes = bytes.to_vec();

    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&write_bytes).await;
        let _ = stdin.shutdown().await;
    });

    let read_fut = async {
        let mut out = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = stdout
                .read(&mut buf)
                .await
                .map_err(|_| PageError::Unreadable)?;
            if n == 0 {
                break;
            }
            let room = PDF_TEXT_BYTES.saturating_sub(out.len());
            if room == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n.min(room)]);
        }
        let status = child.wait().await.map_err(|_| PageError::NoTextLayer)?;
        Ok::<_, PageError>((out, status.success()))
    };

    let res = tokio::time::timeout_at(step_deadline, read_fut).await;
    writer.abort();
    let (out, ok) = match res {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(PageError::Timeout),
    };
    if !ok {
        return Err(PageError::NoTextLayer);
    }
    let markdown = String::from_utf8_lossy(&out).trim().to_owned();
    if markdown.is_empty() {
        return Err(PageError::NoTextLayer);
    }
    Ok(markdown)
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create_private_ocr_dir() -> Result<TempDirGuard, PageError> {
    let pid = std::process::id();
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("quelle-ocr-{pid}-{seq}"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.mode(0o700);
        b.create(&dir).map_err(|_| PageError::NoTextLayer)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&dir).map_err(|_| PageError::NoTextLayer)?;
    }
    Ok(TempDirGuard(dir))
}

/// Apache-2.0 fallback using `pdftoppm` + `tesseract` CLI (~75 MiB peak RSS).
async fn run_tesseract_cli(
    bytes: &[u8],
    max_pages: usize,
    deadline: Instant,
) -> Result<(String, usize), PageError> {
    let _slot = DOCUMENT_SLOTS
        .acquire()
        .await
        .map_err(|_| PageError::Unreadable)?;
    let step_deadline = deadline.min(Instant::now() + OCR_TIMEOUT);
    let work = create_private_ocr_dir()?;
    let pdf_path = work.0.join("input.pdf");
    tokio::fs::write(&pdf_path, bytes)
        .await
        .map_err(|_| PageError::Unreadable)?;

    let prefix = work.0.join("page");
    let cap = max_pages.clamp(1, MAX_PDF_MAX_PAGES).to_string();
    let mut ppm = Command::new("pdftoppm");
    configure_safe_env(&mut ppm);
    let ppm_status = tokio::time::timeout_at(
        step_deadline,
        ppm.args(["-png", "-r", "200", "-f", "1", "-l", &cap])
            .arg(&pdf_path)
            .arg(&prefix)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::NoTextLayer)?;
    if !ppm_status.success() {
        return Err(PageError::NoTextLayer);
    }

    let mut page_files: Vec<PathBuf> = std::fs::read_dir(&work.0)
        .map_err(|_| PageError::NoTextLayer)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("page-") && n.ends_with(".png"))
        })
        .collect();
    page_files.sort();
    if page_files.is_empty() {
        return Err(PageError::NoTextLayer);
    }

    let mut combined = String::new();
    let mut pages_read = 0usize;
    for page_png in page_files {
        if Instant::now() >= step_deadline || combined.len() >= PDF_TEXT_BYTES {
            break;
        }
        let mut tess = Command::new("tesseract");
        configure_safe_env(&mut tess);
        let out = tokio::time::timeout_at(
            step_deadline,
            tess.arg(&page_png)
                .args(["stdout", "--oem", "1", "--psm", "3"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|_| PageError::NoTextLayer)?;
        if out.status.success() {
            let chunk = String::from_utf8_lossy(&out.stdout);
            let trimmed = chunk.trim();
            if !trimmed.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n\n");
                }
                let room = PDF_TEXT_BYTES.saturating_sub(combined.len());
                let end = trimmed.floor_char_boundary(room.min(trimmed.len()));
                combined.push_str(&trimmed[..end]);
            }
            pages_read += 1;
        }
    }

    if combined.trim().is_empty() {
        return Err(PageError::NoTextLayer);
    }
    Ok((combined, pages_read))
}

async fn run_http_ocr_sidecar(
    endpoint: &Url,
    url: &Url,
    bytes: &[u8],
    deadline: Instant,
) -> Result<OcrOutput, PageError> {
    let _slot = DOCUMENT_SLOTS
        .acquire()
        .await
        .map_err(|_| PageError::Unreadable)?;
    let started = Instant::now();
    let client = crate::client_builder()
        .build()
        .map_err(|_| PageError::Transport)?;
    let step_deadline = deadline.min(Instant::now() + OCR_TIMEOUT);
    let response = tokio::time::timeout_at(
        step_deadline,
        client
            .post(endpoint.clone())
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(bytes.to_vec())
            .send(),
    )
    .await
    .map_err(|_| PageError::Timeout)?
    .map_err(|_| PageError::Transport)?;
    if !response.status().is_success() {
        return Err(PageError::NoTextLayer);
    }
    #[derive(Deserialize)]
    struct OcrResponse {
        markdown: Option<String>,
        text: Option<String>,
        pages: Option<usize>,
    }
    let body = tokio::time::timeout_at(step_deadline, read_body(response, PDF_TEXT_BYTES))
        .await
        .map_err(|_| PageError::Timeout)?
        .map_err(|_| PageError::Unreadable)?;
    let parsed: OcrResponse = serde_json::from_slice(&body).map_err(|_| PageError::Unreadable)?;
    let text = parsed
        .markdown
        .or(parsed.text)
        .filter(|t| !t.trim().is_empty())
        .ok_or(PageError::NoTextLayer)?;
    tracing::info!(
        stage = "pdf_ocr",
        host = url.host_str(),
        pages = parsed.pages.unwrap_or(0),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ok = true
    );
    Ok(OcrOutput {
        markdown: text,
        retrieval: "pdf-ocr",
        pages_read: parsed.pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_mode_and_backend_default_to_apache2_tesseract() {
        assert_eq!(LiteparseBackend::default(), LiteparseBackend::Tesseract);
        assert_eq!(OcrMode::parse("auto"), Some(OcrMode::Auto));
        assert_eq!(OcrMode::parse("liteparse"), Some(OcrMode::Liteparse));
        assert_eq!(OcrMode::parse("tesseract"), Some(OcrMode::Tesseract));
        assert_eq!(OcrMode::parse("ocria"), Some(OcrMode::Liteparse));
        assert_eq!(OcrMode::parse("sidecar"), Some(OcrMode::Sidecar));
        assert_eq!(OcrMode::parse("off"), Some(OcrMode::Off));
        assert_eq!(OcrMode::parse("bogus"), None);

        assert_eq!(
            LiteparseBackend::parse("tesseract"),
            Some(LiteparseBackend::Tesseract)
        );
        assert_eq!(
            LiteparseBackend::parse("ocria-nano"),
            Some(LiteparseBackend::OcriaNano)
        );
        assert_eq!(LiteparseBackend::parse("unknown"), None);
    }

    #[test]
    fn liteparse_args_default_to_apache2_tesseract_and_enforce_page_cap() {
        let args = build_liteparse_args(LiteparseBackend::default(), 75);
        assert_eq!(
            args,
            vec![
                "parse",
                "--format",
                "markdown",
                "--ocr-backend",
                "tesseract",
                "--image-mode",
                "off",
                "--max-pages",
                "75",
                "-q",
                "-",
            ]
        );
        let clamped = build_liteparse_args(LiteparseBackend::OcriaNano, 99_999);
        assert_eq!(clamped[4], "ocria-nano");
        assert_eq!(clamped[8], MAX_PDF_MAX_PAGES.to_string());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn liteparse_subprocess_pipes_pdf_stdin_and_isolates_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("quelle-ocr-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("fake-lit.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             if [ -n \"$QUELLE_TOKEN\" ]; then exit 42; fi\n\
             case \"$*\" in *\"--ocr-backend tesseract\"*) ;; *) exit 43 ;; esac\n\
             head=$(head -c 5)\n\
             if [ \"$head\" != \"%PDF-\" ]; then exit 44; fi\n\
             printf '# Scanned Invoice\\n\\nTotal: CHF 420.00\\n'\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let md = run_liteparse_binary(
            &script,
            LiteparseBackend::Tesseract,
            b"%PDF-1.4\nfake-scan-bytes",
            20,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("fake lit ocr");
        assert!(md.contains("# Scanned Invoice"));
        assert!(md.contains("CHF 420.00"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
