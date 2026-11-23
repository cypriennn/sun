use crate::{
    domain::{Attachment, AttachmentKind, Message, SharedFile},
    error::AppError,
    llm::{LlmContentPart, PreparedMessage},
    storage::{Storage, new_id, now_millis},
};
use axum::extract::Multipart;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use xberg::{ExtractInput, ExtractionConfig};

pub fn sanitize_filename(raw: &str) -> Result<String, AppError> {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('.');
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | '\0'))
        .collect();
    let clean = clean.trim();
    if clean.is_empty() || clean == "." || clean == ".." {
        return Err(AppError::BadRequest("invalid file name".to_owned()));
    }
    if clean.chars().count() <= 128 {
        return Ok(clean.to_owned());
    }
    if let Some((stem, ext)) = clean.rsplit_once('.')
        && ext.len() <= 16
    {
        let keep = 127_usize.saturating_sub(ext.len());
        let short_stem: String = stem.chars().take(keep).collect();
        return Ok(format!("{short_stem}.{ext}"));
    }
    Ok(clean.chars().take(128).collect())
}

pub fn detect_image_mime(
    filename: &str,
    declared_mime: Option<&str>,
    head: &[u8],
) -> Option<String> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png".to_owned());
    }
    if head.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg".to_owned());
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif".to_owned());
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some("image/webp".to_owned());
    }
    if head.len() >= 14 && &head[0..2] == b"BM" {
        return Some("image/bmp".to_owned());
    }
    if head.starts_with(b"II*\x00") || head.starts_with(b"MM\x00*") {
        return Some("image/tiff".to_owned());
    }
    if head.starts_with(b"\x00\x00\x01\x00") {
        return Some("image/x-icon".to_owned());
    }
    if head.starts_with(b"\xff\x0a") || head.starts_with(b"\x00\x00\x00\x0cJXL ") {
        return Some("image/jxl".to_owned());
    }
    if head.starts_with(b"qoif") {
        return Some("image/qoi".to_owned());
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        match &head[8..12] {
            b"avif" | b"avis" => return Some("image/avif".to_owned()),
            b"heic" | b"heix" | b"hevc" | b"hevx" => return Some("image/heic".to_owned()),
            b"mif1" | b"msf1" => return Some("image/heif".to_owned()),
            _ => {}
        }
    }

    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let by_ext = match ext.as_str() {
        "png" => Some("image/png"),
        "apng" => Some("image/apng"),
        "jpg" | "jpeg" | "jfif" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" | "svgz" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "ico" | "cur" => Some("image/x-icon"),
        "tif" | "tiff" => Some("image/tiff"),
        "avif" => Some("image/avif"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        "jxl" => Some("image/jxl"),
        "qoi" => Some("image/qoi"),
        "pbm" | "pgm" | "ppm" | "pnm" => Some("image/x-portable-anymap"),
        "tga" => Some("image/x-tga"),
        "dds" => Some("image/vnd-ms.dds"),
        _ => None,
    };
    if let Some(m) = by_ext {
        return Some(m.to_owned());
    }

    declared_mime
        .and_then(|m| m.split(';').next())
        .map(str::trim)
        .filter(|m| m.to_ascii_lowercase().starts_with("image/"))
        .map(str::to_ascii_lowercase)
}

pub fn guess_non_image_mime(filename: &str, declared_mime: Option<&str>) -> String {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let known = match ext.as_str() {
        "pdf" => Some("application/pdf"),
        "md" | "markdown" => Some("text/markdown"),
        "txt" | "log" => Some("text/plain"),
        "yml" | "yaml" => Some("application/yaml"),
        "json" => Some("application/json"),
        "toml" => Some("application/toml"),
        "csv" => Some("text/csv"),
        "tsv" => Some("text/tab-separated-values"),
        "html" | "htm" => Some("text/html"),
        "xml" => Some("application/xml"),
        "zip" => Some("application/zip"),
        "tar" => Some("application/x-tar"),
        "gz" | "tgz" => Some("application/gzip"),
        "7z" => Some("application/x-7z-compressed"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "pptx" => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        "odt" => Some("application/vnd.oasis.opendocument.text"),
        "epub" => Some("application/epub+zip"),
        "rtf" => Some("application/rtf"),
        "eml" => Some("message/rfc822"),
        _ => None,
    };
    if let Some(m) = known {
        return m.to_owned();
    }
    declared_mime
        .and_then(|m| m.split(';').next())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream")
        .to_owned()
}

pub async fn extract_document_text(path: &Path) -> Option<String> {
    let cfg = ExtractionConfig {
        use_cache: false,
        disable_ocr: true,
        ..Default::default()
    };
    let uri = path.to_string_lossy().into_owned();
    if let Ok(res) = xberg::extract(ExtractInput::from_uri(uri), &cfg).await {
        let parts: Vec<&str> = res
            .results
            .iter()
            .map(|d| d.content.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n\n"));
        }
    }

    let meta = tokio::fs::metadata(path).await.ok()?;
    if meta.len() == 0 || meta.len() > 8 * 1024 * 1024 {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(&bytes)
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn extracted_cache_path(extracted_dir: &Path, safe_name: &str) -> PathBuf {
    extracted_dir.join(format!("{safe_name}.txt"))
}

async fn classify_stored_file(
    safe_name: &str,
    head: &[u8],
    extracted_dir: &Path,
) -> (String, AttachmentKind) {
    if let Some(img_mime) = detect_image_mime(safe_name, None, head) {
        return (img_mime, AttachmentKind::Image);
    }
    let mime = guess_non_image_mime(safe_name, None);
    let has_extracted = tokio::fs::metadata(extracted_cache_path(extracted_dir, safe_name))
        .await
        .is_ok();
    let kind = if has_extracted {
        AttachmentKind::Document
    } else {
        AttachmentKind::File
    };
    (mime, kind)
}

pub async fn upload_multipart(
    storage: &Storage,
    user_id: &str,
    chat_id: &str,
    max_file_size: u64,
    mut multipart: Multipart,
) -> Result<Vec<SharedFile>, AppError> {
    let _ = storage.get_chat(user_id, chat_id).await?;
    let shared_dir = storage.shared_dir(user_id, chat_id)?;
    let extracted_dir = storage.extracted_dir(user_id, chat_id)?;
    tokio::fs::create_dir_all(&shared_dir).await?;
    tokio::fs::create_dir_all(&extracted_dir).await?;

    let mut uploaded = Vec::new();

    while let Some(mut field) = multipart.next_field().await? {
        let Some(raw_name) = field.file_name().map(str::to_owned) else {
            continue;
        };
        let safe_name = sanitize_filename(&raw_name)?;
        let declared_mime = field.content_type().map(str::to_owned);

        let tmp_path = shared_dir.join(format!(".upload.{}.tmp", new_id()));
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut size: u64 = 0;
        let mut head = Vec::with_capacity(64);

        let stream_res: Result<(), AppError> = async {
            while let Some(chunk) = field.chunk().await? {
                size = size.saturating_add(chunk.len() as u64);
                if size > max_file_size {
                    return Err(AppError::PayloadTooLarge(format!(
                        "file '{safe_name}' exceeds maximum allowed size of {max_file_size} bytes"
                    )));
                }
                if head.len() < 64 {
                    let take = (64 - head.len()).min(chunk.len());
                    head.extend_from_slice(&chunk[..take]);
                }
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            Ok(())
        }
        .await;

        drop(file);
        if let Err(err) = stream_res {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(err);
        }

        let final_path = shared_dir.join(&safe_name);
        tokio::fs::rename(&tmp_path, &final_path).await?;

        let cache_path = extracted_cache_path(&extracted_dir, &safe_name);
        let (mime_type, kind) = if let Some(img_mime) =
            detect_image_mime(&safe_name, declared_mime.as_deref(), &head)
        {
            let _ = tokio::fs::remove_file(&cache_path).await;
            (img_mime, AttachmentKind::Image)
        } else {
            let mime = guess_non_image_mime(&safe_name, declared_mime.as_deref());
            if let Some(text) = extract_document_text(&final_path).await {
                let tmp_txt = extracted_dir.join(format!(".ext.{}.tmp", new_id()));
                tokio::fs::write(&tmp_txt, text.as_bytes()).await?;
                tokio::fs::rename(&tmp_txt, &cache_path).await?;
                (mime, AttachmentKind::Document)
            } else {
                let _ = tokio::fs::remove_file(&cache_path).await;
                (mime, AttachmentKind::File)
            }
        };

        uploaded.push(SharedFile {
            name: safe_name,
            mime_type,
            size,
            kind,
            updated_at: now_millis(),
        });
    }

    if uploaded.is_empty() {
        return Err(AppError::BadRequest("no files provided".to_owned()));
    }

    Ok(uploaded)
}

pub async fn list_shared_files(
    storage: &Storage,
    user_id: &str,
    chat_id: &str,
) -> Result<Vec<SharedFile>, AppError> {
    let shared_dir = storage.shared_dir(user_id, chat_id)?;
    let extracted_dir = storage.extracted_dir(user_id, chat_id)?;
    let mut dir = match tokio::fs::read_dir(&shared_dir).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(AppError::Internal(e.to_string())),
    };

    let mut items = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata().await?;
        if !meta.is_file() {
            continue;
        }
        let head = read_head(&entry.path()).await;
        let updated_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let (mime_type, kind) = classify_stored_file(&name, &head, &extracted_dir).await;

        items.push(SharedFile {
            name,
            mime_type,
            size: meta.len(),
            kind,
            updated_at,
        });
    }

    items.sort_by_key(|f| Reverse(f.updated_at));
    Ok(items)
}

pub async fn resolve_shared_file_attachment(
    storage: &Storage,
    user_id: &str,
    chat_id: &str,
    raw_name: &str,
) -> Result<Attachment, AppError> {
    let safe_name = sanitize_filename(raw_name)?;
    let path = storage.shared_dir(user_id, chat_id)?.join(&safe_name);
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|_| AppError::NotFound(format!("shared file '{safe_name}' not found")))?;
    let head = read_head(&path).await;
    let extracted_dir = storage.extracted_dir(user_id, chat_id)?;
    let (mime_type, kind) = classify_stored_file(&safe_name, &head, &extracted_dir).await;

    Ok(Attachment {
        name: safe_name,
        mime_type,
        size: meta.len(),
        kind,
    })
}

pub async fn read_shared_file(
    storage: &Storage,
    user_id: &str,
    chat_id: &str,
    raw_name: &str,
) -> Result<(String, String, Vec<u8>), AppError> {
    let _ = storage.get_chat(user_id, chat_id).await?;
    let safe_name = sanitize_filename(raw_name)?;
    if safe_name != raw_name {
        return Err(AppError::BadRequest("invalid file name".to_owned()));
    }
    let path = storage.shared_dir(user_id, chat_id)?.join(&safe_name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| AppError::NotFound("file not found".to_owned()))?;
    let head = &bytes[..bytes.len().min(64)];
    let mime = detect_image_mime(&safe_name, None, head)
        .unwrap_or_else(|| guess_non_image_mime(&safe_name, None));
    Ok((safe_name, mime, bytes))
}

pub async fn delete_shared_file(
    storage: &Storage,
    user_id: &str,
    chat_id: &str,
    raw_name: &str,
) -> Result<(), AppError> {
    let _ = storage.get_chat(user_id, chat_id).await?;
    let safe_name = sanitize_filename(raw_name)?;
    if safe_name != raw_name {
        return Err(AppError::BadRequest("invalid file name".to_owned()));
    }
    let shared_path = storage.shared_dir(user_id, chat_id)?.join(&safe_name);
    let cache_path = extracted_cache_path(&storage.extracted_dir(user_id, chat_id)?, &safe_name);
    let _ = tokio::fs::remove_file(&cache_path).await;
    match tokio::fs::remove_file(&shared_path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(AppError::NotFound("file not found".to_owned()))
        }
        Err(e) => Err(AppError::Internal(e.to_string())),
    }
}

pub async fn prepare_messages(
    storage: &Storage,
    user_id: &str,
    chat_id: &str,
    messages: &[Message],
) -> Vec<PreparedMessage> {
    let (Ok(shared_dir), Ok(extracted_dir)) = (
        storage.shared_dir(user_id, chat_id),
        storage.extracted_dir(user_id, chat_id),
    ) else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.attachments.is_empty() {
            out.push(PreparedMessage {
                role: msg.role,
                text: msg.content.clone(),
                parts: Vec::new(),
                reasoning_items: msg.reasoning_items.clone(),
                tool_calls: msg.tool_calls.clone(),
                tool_call_id: msg.tool_call_id.clone(),
            });
            continue;
        }

        let mut parts = Vec::with_capacity(msg.attachments.len() + 1);
        for att in &msg.attachments {
            let file_path = shared_dir.join(&att.name);
            match att.kind {
                AttachmentKind::Image => {
                    if let Ok(bytes) = tokio::fs::read(&file_path).await {
                        let b64 = BASE64.encode(&bytes);
                        parts.push(LlmContentPart::ImageUrl(format!(
                            "data:{};base64,{b64}",
                            att.mime_type
                        )));
                    }
                }
                AttachmentKind::Document => {
                    let cache = extracted_cache_path(&extracted_dir, &att.name);
                    let text = match tokio::fs::read_to_string(&cache).await {
                        Ok(t) => Some(t),
                        Err(_) => extract_document_text(&file_path).await,
                    };
                    if let Some(t) = text {
                        parts.push(LlmContentPart::Text(format!("[File: {}]\n{t}", att.name)));
                    }
                }
                AttachmentKind::File => {
                    parts.push(LlmContentPart::Text(format!(
                        "[Shared file: {} ({}, {} bytes)]",
                        att.name, att.mime_type, att.size
                    )));
                }
            }
        }
        if !msg.content.is_empty() {
            parts.push(LlmContentPart::Text(msg.content.clone()));
        }

        out.push(PreparedMessage {
            role: msg.role,
            text: String::new(),
            parts,
            reasoning_items: msg.reasoning_items.clone(),
            tool_calls: msg.tool_calls.clone(),
            tool_call_id: msg.tool_call_id.clone(),
        });
    }
    out
}

async fn read_head(path: &Path) -> Vec<u8> {
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return Vec::new();
    };
    let mut buf = vec![0_u8; 64];
    let n = f.read(&mut buf).await.unwrap_or(0);
    buf.truncate(n);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_stored_zip(entry_name: &str, data: &[u8]) -> Vec<u8> {
        let crc = crc32(data);
        let name_b = entry_name.as_bytes();
        let name_len = u16::try_from(name_b.len()).expect("name len");
        let size = u32::try_from(data.len()).expect("data len");

        let mut out = Vec::new();
        out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        out.extend_from_slice(&20_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(name_b);
        out.extend_from_slice(data);

        let cd_offset = u32::try_from(out.len()).expect("cd offset");
        out.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        out.extend_from_slice(&20_u16.to_le_bytes());
        out.extend_from_slice(&20_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u32.to_le_bytes());
        out.extend_from_slice(&0_u32.to_le_bytes());
        out.extend_from_slice(name_b);

        let cd_size = u32::try_from(out.len()).expect("end") - cd_offset;
        out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffff_u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn minimal_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let objs = [
            "1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n".to_owned(),
            "2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n".to_owned(),
            "3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >> endobj\n".to_owned(),
            format!(
                "4 0 obj << /Length {} >> stream\n{stream}\nendstream endobj\n",
                stream.len()
            ),
            "5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\n".to_owned(),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::with_capacity(objs.len());
        for obj in &objs {
            offsets.push(pdf.len());
            pdf.push_str(obj);
        }
        let xref_pos = pdf.len();
        pdf.push_str("xref\n0 6\n0000000000 65535 f \n");
        for off in offsets {
            pdf.push_str(&format!("{off:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer << /Size 6 /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n"
        ));
        pdf.into_bytes()
    }

    #[test]
    fn detects_all_image_formats_and_sanitizes_names() {
        assert_eq!(
            detect_image_mime("photo.heic", None, b"").as_deref(),
            Some("image/heic")
        );
        assert_eq!(
            detect_image_mime("raw.bin", None, b"\x89PNG\r\n\x1a\nxxxx").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            detect_image_mime("pic", Some("image/webp; charset=binary"), b"").as_deref(),
            Some("image/webp")
        );
        assert!(detect_image_mime("notes.md", None, b"# title").is_none());
        assert_eq!(
            sanitize_filename("../../etc/passwd").expect("sanitized"),
            "passwd"
        );
        assert!(sanitize_filename("...").is_err());
    }

    #[tokio::test]
    async fn extracts_pdf_markdown_yaml_and_zip_with_xberg() {
        let dir = PathBuf::from(format!("/tmp/opencode/sun-xberg-{}", new_id()));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");

        let md_path = dir.join("guide.md");
        tokio::fs::write(&md_path, "# Ops Guide\nDeploy fast.")
            .await
            .expect("write md");
        assert!(
            extract_document_text(&md_path)
                .await
                .expect("extract md")
                .contains("Deploy fast")
        );

        let yml_path = dir.join("service.yml");
        tokio::fs::write(&yml_path, "replicas: 3\nregion: eu-west\n")
            .await
            .expect("write yml");
        assert!(
            extract_document_text(&yml_path)
                .await
                .expect("extract yml")
                .contains("eu-west")
        );

        let pdf_path = dir.join("runbook.pdf");
        tokio::fs::write(&pdf_path, minimal_pdf("Kernel tuning verified"))
            .await
            .expect("write pdf");
        assert!(
            extract_document_text(&pdf_path)
                .await
                .expect("extract pdf")
                .contains("Kernel tuning verified")
        );

        let zip_path = dir.join("bundle.zip");
        tokio::fs::write(
            &zip_path,
            minimal_stored_zip("notes.txt", b"Archive secret payload"),
        )
        .await
        .expect("write zip");
        let zip_txt = extract_document_text(&zip_path).await.expect("extract zip");
        assert!(zip_txt.contains("notes.txt"));
        assert!(zip_txt.contains("Archive secret payload"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
