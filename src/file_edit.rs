//! UTF-8 edits with preconditions and byte-preserving replacements.
use crate::tools::{Result, ToolError};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub static WRITES: Mutex<()> = Mutex::new(());
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn workspace_path(root: &Path, raw: &str) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let input = Path::new(raw);
    if !input.is_absolute()
        && input
            .components()
            .any(|part| matches!(part, Component::Prefix(_) | Component::RootDir))
    {
        return Err(ToolError::AbsolutePath(raw.into()));
    }
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    if candidate
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(ToolError::ParentTraversal(raw.into()));
    }
    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        // A dangling symlink must not be treated as a new regular destination.
        if fs::symlink_metadata(ancestor).is_ok() {
            return Err(ToolError::WriteConflict(
                "dangling link in destination".into(),
            ));
        }
        ancestor = ancestor
            .parent()
            .ok_or_else(|| ToolError::OutsideWorkspace(raw.into()))?;
    }
    let resolved = ancestor.canonicalize()?;
    if !resolved.starts_with(&root) {
        return Err(ToolError::OutsideWorkspace(raw.into()));
    }
    let suffix = candidate
        .strip_prefix(ancestor)
        .map_err(|_| ToolError::OutsideWorkspace(raw.into()))?;
    // Joining an empty suffix adds a trailing separator to an existing file on
    // Windows, so it is then treated as a directory (ERROR_DIRECTORY / 267).
    Ok(if suffix.as_os_str().is_empty() { resolved } else { resolved.join(suffix) })
}

pub fn snapshot(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn check_hash(original: Option<&[u8]>, expected: Option<&str>) -> Result<()> {
    if let Some(expected) = expected {
        let actual = original.map(crate::symbol_description::content_hash);
        if actual.as_deref() != Some(expected) {
            return Err(ToolError::WriteConflict(format!(
                "expected_code_hash={expected}, current_code_hash={}; retrieve current source before retrying",
                actual.as_deref().unwrap_or("missing")
            )));
        }
    }
    Ok(())
}

pub fn commit(path: &Path, original: Option<&[u8]>, content: &[u8]) -> Result<bool> {
    if snapshot(path)?.as_deref() != original {
        return Err(ToolError::WriteConflict(
            "file changed since it was read; no edit was applied".into(),
        ));
    }
    if original == Some(content) {
        return Ok(false);
    }
    if path.file_name().is_none() {
        return Err(ToolError::NotFile(path.display().to_string()));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = path.with_file_name(format!(
        ".agent-write-{}-{stamp}-{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut created = false;
    let outcome = (|| -> Result<()> {
        let permissions = fs::metadata(path).ok().map(|meta| meta.permissions());
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        created = true;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        if let Some(permissions) = permissions {
            fs::set_permissions(&tmp, permissions)?;
        }
        // The process-wide write lock covers direct MCP and Agent writes. Also
        // reject external changes observed before replacing the destination.
        if snapshot(path)?.as_deref() != original {
            return Err(ToolError::WriteConflict(
                "file changed before commit; no edit was applied".into(),
            ));
        }
        if original.is_none() {
            // Publishing a new file must not overwrite an external creator.
            publish_new(&tmp, path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    ToolError::WriteConflict(
                        "destination was created before commit; no edit was applied".into(),
                    )
                } else {
                    error.into()
                }
            })?;
        } else {
            fs::rename(&tmp, path)?;
        }
        Ok(())
    })();
    if outcome.is_err() && created {
        #[cfg(windows)]
        if let Ok(meta) = fs::metadata(&tmp) {
            let mut permissions = meta.permissions();
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&tmp, permissions);
        }
        let _ = fs::remove_file(&tmp);
    }
    outcome?;
    crate::source_read::invalidate(path);
    Ok(true)
}

#[cfg(windows)]
fn publish_new(tmp: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let from = tmp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let to = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // No REPLACE_EXISTING flag: an external creator must cause a conflict.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 8) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(not(windows))]
fn publish_new(tmp: &Path, path: &Path) -> std::io::Result<()> {
    fs::hard_link(tmp, path)?;
    if let Err(error) = fs::remove_file(tmp) {
        tracing::warn!(%error,"new file saved; temporary hard link could not be removed");
    }
    Ok(())
}

fn newline(source: &str) -> &'static str {
    match source.find('\n') {
        Some(index) if index > 0 && source.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}
fn normalize(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}
pub fn line_replacement(
    source: &str,
    start: usize,
    end: usize,
    replacement: &str,
    expected: Option<&str>,
) -> Result<String> {
    let spans = source
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let begin = *offset;
            *offset += line.len();
            Some((begin, *offset))
        })
        .collect::<Vec<_>>();
    if start == 0 || end < start || end > spans.len() {
        return Err(ToolError::InvalidLineRange { start, end });
    }
    let (begin, finish) = (spans[start - 1].0, spans[end - 1].1);
    let selected = &source[begin..finish];
    let selected_text = normalize(selected);
    if let Some(expected) = expected {
        let expected = normalize(expected);
        if expected.strip_suffix('\n').unwrap_or(&expected)
            != selected_text.strip_suffix('\n').unwrap_or(&selected_text)
        {
            return Err(ToolError::ExpectedTextMismatch);
        }
    }
    let ending = newline(if selected.contains('\n') {
        selected
    } else {
        source
    });
    let mut inserted = normalize(replacement).replace('\n', ending);
    if !inserted.is_empty()
        && !inserted.ends_with('\n')
        && (finish < source.len() || selected.ends_with('\n'))
    {
        inserted.push_str(ending);
    }
    let mut output = String::with_capacity(source.len() + inserted.len());
    output.push_str(&source[..begin]);
    output.push_str(&inserted);
    output.push_str(&source[finish..]);
    Ok(output)
}

pub fn text_edits(source: &str, edits: &[crate::tools::TextEdit]) -> Result<String> {
    if edits.is_empty() || edits.len() > 100 {
        return Err(ToolError::InvalidEdit(
            "provide between 1 and 100 edits".into(),
        ));
    }
    let ending = newline(source);
    let mut replacements = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(ToolError::InvalidEdit(format!(
                "edit {index}: old_text must be a nonempty unique anchor; insert by retaining the anchor in new_text"
            )));
        }
        // Model excerpts commonly use LF for a CRLF file. Match either the
        // literal bytes or that exact newline-normalized anchor, never regex.
        let normalized = normalize(&edit.old_text).replace('\n', ending);
        let anchor = if source.contains(&edit.old_text) {
            &edit.old_text
        } else {
            &normalized
        };
        let mut offsets = source.match_indices(anchor);
        let Some((start, _)) = offsets.next() else {
            return Err(ToolError::InvalidEdit(format!(
                "edit {index}: old_text not found; no edits applied"
            )));
        };
        let next_char = start + anchor.chars().next().unwrap().len_utf8();
        if offsets.next().is_some() || source[next_char..].contains(anchor) {
            return Err(ToolError::InvalidEdit(format!(
                "edit {index}: old_text is ambiguous; supply more surrounding text; no edits applied"
            )));
        }
        replacements.push((
            start,
            start + anchor.len(),
            normalize(&edit.new_text).replace('\n', ending),
        ));
    }
    replacements.sort_by_key(|edit| edit.0);
    if replacements.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(ToolError::InvalidEdit(
            "edit anchors overlap; no edits applied".into(),
        ));
    }
    let mut output = source.to_owned();
    for (begin, end, text) in replacements.into_iter().rev() {
        output.replace_range(begin..end, &text);
    }
    Ok(output)
}
