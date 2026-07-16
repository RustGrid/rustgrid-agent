use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    api::RustGridClient,
    manifest::{ExecutionManifest, ManifestAttachment, ManifestAttachmentVariant},
};

const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_ATTACHMENT_CONTEXT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PREVIEW_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug)]
pub struct StagedAttachment {
    pub filename: String,
    pub mime: Option<String>,
    pub relative_path: PathBuf,
    pub image_path: Option<PathBuf>,
}

pub fn stage(
    api: &RustGridClient,
    manifest: &ExecutionManifest,
    repo_root: &Path,
) -> Result<Vec<StagedAttachment>> {
    let context_root = repo_root.join(".git/rustgrid-agent/context/attachments");
    if context_root.exists() {
        fs::remove_dir_all(&context_root)
            .with_context(|| format!("could not clear {}", context_root.display()))?;
    }
    if manifest.attachments.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(&context_root)
        .with_context(|| format!("could not create {}", context_root.display()))?;

    let mut total_bytes = 0u64;
    let mut staged = Vec::with_capacity(manifest.attachments.len());
    for (index, attachment) in manifest.attachments.iter().enumerate() {
        let declared_size = attachment.size_bytes.unwrap_or(MAX_ATTACHMENT_BYTES as i64);
        if declared_size <= 0 || declared_size as u64 > MAX_ATTACHMENT_BYTES {
            bail!(
                "attachment {} exceeds the worker attachment size limit",
                attachment.id
            );
        }
        let bytes = api.download_attachment(&attachment.id, MAX_ATTACHMENT_BYTES)?;
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        enforce_context_limit(total_bytes)?;
        verify_sha256(attachment, &bytes)?;
        let relative_path = PathBuf::from(format!(
            ".git/rustgrid-agent/context/attachments/{:02}-{}",
            index + 1,
            safe_filename(&attachment.filename)
        ));
        let absolute_path = repo_root.join(&relative_path);
        fs::write(&absolute_path, &bytes)
            .with_context(|| format!("could not stage attachment {}", attachment.id))?;

        let image_path = if is_codex_image_mime(attachment.mime.as_deref()) {
            Some(absolute_path)
        } else if let Some(variant) = image_variant(attachment) {
            let preview =
                api.download_attachment_variant(&attachment.id, &variant.kind, MAX_PREVIEW_BYTES)?;
            total_bytes = total_bytes.saturating_add(preview.len() as u64);
            enforce_context_limit(total_bytes)?;
            let preview_path = context_root.join(format!(
                "{:02}-{}.preview.{}",
                index + 1,
                safe_stem(&attachment.filename),
                extension_for_mime(&variant.mime)
            ));
            fs::write(&preview_path, preview).with_context(|| {
                format!("could not stage preview for attachment {}", attachment.id)
            })?;
            Some(preview_path)
        } else {
            None
        };

        staged.push(StagedAttachment {
            filename: attachment.filename.clone(),
            mime: attachment.mime.clone(),
            relative_path,
            image_path,
        });
    }
    Ok(staged)
}

fn enforce_context_limit(total_bytes: u64) -> Result<()> {
    if total_bytes > MAX_ATTACHMENT_CONTEXT_BYTES {
        bail!("ticket attachment context exceeds the worker aggregate size limit");
    }
    Ok(())
}

pub fn prompt_section(attachments: &[StagedAttachment]) -> Option<String> {
    if attachments.is_empty() {
        return None;
    }
    let mut section = String::from(
        "Ticket attachments were downloaded and verified by the worker. Treat them as additional task context:\n",
    );
    for attachment in attachments {
        section.push_str(&format!(
            "- {} ({}) at `{}`{}\n",
            attachment.filename,
            attachment.mime.as_deref().unwrap_or("unknown type"),
            attachment.relative_path.display(),
            if attachment.image_path.is_some() {
                " (also attached to the initial Codex prompt as an image)"
            } else {
                ""
            }
        ));
    }
    Some(section)
}

fn verify_sha256(attachment: &ManifestAttachment, bytes: &[u8]) -> Result<()> {
    let Some(expected) = attachment.sha256.as_deref() else {
        return Ok(());
    };
    let actual = hex::encode(Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("attachment {} failed SHA-256 verification", attachment.id);
    }
    Ok(())
}

fn image_variant(attachment: &ManifestAttachment) -> Option<&ManifestAttachmentVariant> {
    let priorities: &[&str] = match attachment.media_family.as_str() {
        "pdf" => &["pdf_page_1_web"],
        "image" => &["preview_web", "thumb_256"],
        _ => &[],
    };
    priorities.iter().find_map(|kind| {
        attachment.variants.iter().find(|variant| {
            variant.ready && variant.kind == *kind && is_codex_image_mime(Some(&variant.mime))
        })
    })
}

fn is_codex_image_mime(mime: Option<&str>) -> bool {
    matches!(mime, Some("image/png" | "image/jpeg"))
}

fn extension_for_mime(mime: &str) -> &'static str {
    if mime == "image/png" { "png" } else { "jpg" }
}

fn safe_filename(filename: &str) -> String {
    let basename = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let sanitized = basename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim_matches('.').trim();
    if sanitized.is_empty() {
        "attachment".to_owned()
    } else {
        sanitized.chars().take(180).collect()
    }
}

fn safe_stem(filename: &str) -> String {
    let safe = safe_filename(filename);
    Path::new(&safe)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("attachment")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_untrusted_attachment_names() {
        assert_eq!(
            safe_filename("../../UI screenshot (1).png"),
            "UI_screenshot__1_.png"
        );
        assert_eq!(safe_filename(".."), "attachment");
    }

    #[test]
    fn selects_pdf_and_image_preview_variants() {
        let mut attachment = ManifestAttachment {
            id: uuid::Uuid::new_v4().to_string(),
            ticket_id: uuid::Uuid::new_v4().to_string(),
            filename: "report.pdf".into(),
            mime: Some("application/pdf".into()),
            media_family: "pdf".into(),
            size_bytes: Some(10),
            sha256: None,
            status: "ready".into(),
            virus_status: "clean".into(),
            variants: vec![ManifestAttachmentVariant {
                kind: "pdf_page_1_web".into(),
                mime: "image/jpeg".into(),
                ready: true,
            }],
        };
        assert_eq!(image_variant(&attachment).unwrap().kind, "pdf_page_1_web");
        attachment.media_family = "text".into();
        assert!(image_variant(&attachment).is_none());
    }
}
