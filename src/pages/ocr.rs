//! PDF OCR fallback supporting LlamaIndex OCRia-nano-1 via LiteParse (`lit`)
//! and optional HTTP OCR sidecars (`QUELLE_OCR_URL`).

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OcrMode {
    #[default]
    Auto,
    Ocria,
    Sidecar,
    Off,
}

impl OcrMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "ocria" | "ocria-nano" | "liteparse" => Some(Self::Ocria),
            "sidecar" | "http" => Some(Self::Sidecar),
            "off" | "none" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LiteparseBackend {
    #[default]
    OcriaNano,
    Tesseract,
}

impl LiteparseBackend {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ocria" | "ocria-nano" | "ocria-nano-1" => Some(Self::OcriaNano),
            "tesseract" => Some(Self::Tesseract),
            _ => None,
        }
    }

    pub fn as_arg(self) -> &'static str {
        match self {
            Self::OcriaNano => "ocria-nano",
            Self::Tesseract => "tesseract",
        }
    }

    pub fn retrieval_label(self) -> &'static str {
        match self {
            Self::OcriaNano => "pdf-ocria",
            Self::Tesseract => "pdf-tesseract",
        }
    }
}

static OCR_MODE: OnceLock<OcrMode> = OnceLock::new();
static LITEPARSE_BACKEND: OnceLock<LiteparseBackend> = OnceLock::new();
static LITEPARSE_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
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
        .unwrap_or(LiteparseBackend::OcriaNano)
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

fn liteparse_bin() -> Option<&'static Path> {
    LITEPARSE_BIN.get().and_then(Option::as_deref)
}

pub(crate) fn any_ocr_enabled() -> bool {
    match ocr_mode() {
        OcrMode::Off => false,
        OcrMode::Ocria => liteparse_bin().is_some(),
        OcrMode::Sidecar => ocr_sidecar().is_some(),
        OcrMode::Auto => liteparse_bin().is_some() || ocr_sidecar().is_some(),
    }
}

/// Builds the exact `lit parse` CLI argument list for OCRia-nano-1 / LiteParse.
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

/// Safe environment allowlist forwarded to LiteParse so it can locate cached
/// OCRia-nano-1 ONNX weights and runtime libraries without inheriting Quelle
/// secrets such as `QUELLE_TOKEN` or API keys.
const SAFE_LITEPARSE_ENV: &[&str] = &[
    "HOME",
    "XDG_CACHE_HOME",
    "LITEPARSE_MODEL_DIR",
    "ORT_DYLIB_PATH",
    "TESSDATA_PREFIX",
    "PATH",
    "TMPDIR",
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

    if matches!(mode, OcrMode::Ocria | OcrMode::Auto)
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
            Err(err) if mode == OcrMode::Ocria || ocr_sidecar().is_none() => {
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

    if matches!(mode, OcrMode::Sidecar | OcrMode::Auto)
        && let Some(endpoint) = ocr_sidecar()
    {
        return run_http_ocr_sidecar(endpoint, url, bytes, deadline).await;
    }

    Err(PageError::NoTextLayer)
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
    command.env_clear();
    for key in SAFE_LITEPARSE_ENV {
        if let Some(val) = std::env::var_os(key) {
            command.env(key, val);
        }
    }
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
    fn ocr_mode_and_backend_parse_cleanly() {
        assert_eq!(OcrMode::parse("auto"), Some(OcrMode::Auto));
        assert_eq!(OcrMode::parse("ocria"), Some(OcrMode::Ocria));
        assert_eq!(OcrMode::parse("ocria-nano"), Some(OcrMode::Ocria));
        assert_eq!(OcrMode::parse("sidecar"), Some(OcrMode::Sidecar));
        assert_eq!(OcrMode::parse("off"), Some(OcrMode::Off));
        assert_eq!(OcrMode::parse("bogus"), None);

        assert_eq!(
            LiteparseBackend::parse("ocria-nano"),
            Some(LiteparseBackend::OcriaNano)
        );
        assert_eq!(
            LiteparseBackend::parse("ocria-nano-1"),
            Some(LiteparseBackend::OcriaNano)
        );
        assert_eq!(
            LiteparseBackend::parse("tesseract"),
            Some(LiteparseBackend::Tesseract)
        );
        assert_eq!(LiteparseBackend::parse("unknown"), None);
    }

    #[test]
    fn liteparse_args_use_ocria_nano_markdown_and_page_cap() {
        let args = build_liteparse_args(LiteparseBackend::OcriaNano, 75);
        assert_eq!(
            args,
            vec![
                "parse",
                "--format",
                "markdown",
                "--ocr-backend",
                "ocria-nano",
                "--image-mode",
                "off",
                "--max-pages",
                "75",
                "-q",
                "-",
            ]
        );
        // Enforces bounds.
        let clamped = build_liteparse_args(LiteparseBackend::OcriaNano, 99_999);
        assert_eq!(clamped[8], MAX_PDF_MAX_PAGES.to_string());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn liteparse_subprocess_pipes_pdf_stdin_and_isolates_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("quelle-ocr-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("fake-lit.sh");
        // Verifies stdin starts with %PDF-, args include ocria-nano, and QUELLE_TOKEN is NOT leaked.
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             if [ -n \"$QUELLE_TOKEN\" ]; then exit 42; fi\n\
             case \"$*\" in *\"--ocr-backend ocria-nano\"*) ;; *) exit 43 ;; esac\n\
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
            LiteparseBackend::OcriaNano,
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
