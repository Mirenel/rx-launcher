use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

pub const PATCH_MAGIC: &[u8; 4] = b"RXBP";
pub const PATCH_VERSION: u8 = 1;
pub const PATCH_HEADER_SIZE: usize = 20;
pub const PATCH_MAX_HUNKS: usize = 4096;
pub const PATCH_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct PatchHunk {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

pub fn build_patch(source: &[u8], target: &[u8]) -> Result<Vec<u8>, String> {
    if source.len() != target.len() {
        return Err("Executable patch requires source and target files of equal size".into());
    }

    let mut hunks = Vec::new();
    let mut index = 0usize;
    while index < source.len() {
        if source[index] == target[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < source.len() && source[index] != target[index] {
            index += 1;
        }
        hunks.push(PatchHunk {
            offset: start as u64,
            bytes: target[start..index].to_vec(),
        });
    }

    encode_patch(source.len() as u64, &hunks)
}

pub fn encode_patch(source_size: u64, hunks: &[PatchHunk]) -> Result<Vec<u8>, String> {
    if hunks.len() > PATCH_MAX_HUNKS {
        return Err("Executable patch contains too many replacement ranges".into());
    }

    let mut output = Vec::with_capacity(PATCH_HEADER_SIZE);
    output.extend_from_slice(PATCH_MAGIC);
    output.push(PATCH_VERSION);
    output.extend_from_slice(&[0, 0, 0]);
    output.extend_from_slice(&source_size.to_le_bytes());
    output.extend_from_slice(&(hunks.len() as u32).to_le_bytes());

    let mut previous_end = 0u64;
    for (index, hunk) in hunks.iter().enumerate() {
        if hunk.bytes.is_empty() {
            return Err(format!("Executable patch hunk {index} is empty"));
        }
        let end = hunk
            .offset
            .checked_add(hunk.bytes.len() as u64)
            .ok_or_else(|| format!("Executable patch hunk {index} overflows"))?;
        if hunk.offset < previous_end || end > source_size {
            return Err(format!(
                "Executable patch hunk {index} is out of bounds or overlaps"
            ));
        }
        previous_end = end;
        output.extend_from_slice(&hunk.offset.to_le_bytes());
        output.extend_from_slice(&(hunk.bytes.len() as u32).to_le_bytes());
        output.extend_from_slice(&hunk.bytes);
        if output.len() > PATCH_MAX_BYTES {
            return Err("Executable patch is larger than the permitted limit".into());
        }
    }

    Ok(output)
}

pub fn parse_patch(bytes: &[u8], expected_source_size: u64) -> Result<Vec<PatchHunk>, String> {
    if bytes.len() < PATCH_HEADER_SIZE || bytes.len() > PATCH_MAX_BYTES {
        return Err("Executable patch has an invalid size".into());
    }
    if &bytes[..4] != PATCH_MAGIC || bytes[4] != PATCH_VERSION || bytes[5..8] != [0, 0, 0] {
        return Err("Executable patch has an unsupported format".into());
    }
    let source_size = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if source_size != expected_source_size {
        return Err("Executable patch does not match this client size".into());
    }
    let hunk_count = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    if hunk_count > PATCH_MAX_HUNKS {
        return Err("Executable patch contains too many replacement ranges".into());
    }

    let mut hunks = Vec::with_capacity(hunk_count);
    let mut cursor = PATCH_HEADER_SIZE;
    let mut previous_end = 0u64;
    for index in 0..hunk_count {
        if bytes.len().saturating_sub(cursor) < 12 {
            return Err(format!("Executable patch hunk {index} is truncated"));
        }
        let offset = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        let length =
            u32::from_le_bytes(bytes[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
        cursor += 12;
        if length == 0 || bytes.len().saturating_sub(cursor) < length {
            return Err(format!("Executable patch hunk {index} is truncated"));
        }
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| format!("Executable patch hunk {index} overflows"))?;
        if offset < previous_end || end > source_size {
            return Err(format!(
                "Executable patch hunk {index} is out of bounds or overlaps"
            ));
        }
        previous_end = end;
        hunks.push(PatchHunk {
            offset,
            bytes: bytes[cursor..cursor + length].to_vec(),
        });
        cursor += length;
    }
    if cursor != bytes.len() {
        return Err("Executable patch contains trailing data".into());
    }
    Ok(hunks)
}

pub fn apply_patch_bytes(source: &[u8], patch: &[u8]) -> Result<Vec<u8>, String> {
    let hunks = parse_patch(patch, source.len() as u64)?;
    let mut output = source.to_vec();
    for hunk in hunks {
        let start = hunk.offset as usize;
        let end = start + hunk.bytes.len();
        output[start..end].copy_from_slice(&hunk.bytes);
    }
    Ok(output)
}

pub fn apply_patch(path: &Path, patch: &[u8], expected_source_size: u64) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| "Could not open the temporary executable for patching".to_string())?;
    let actual_size = file
        .metadata()
        .map_err(|_| "Could not inspect the temporary executable".to_string())?
        .len();
    if actual_size != expected_source_size {
        return Err("The temporary executable size changed before patching".into());
    }
    let hunks = parse_patch(patch, expected_source_size)?;
    for hunk in hunks {
        file.seek(SeekFrom::Start(hunk.offset))
            .map_err(|_| "Could not seek in the temporary executable".to_string())?;
        file.write_all(&hunk.bytes)
            .map_err(|_| "Could not apply the executable patch".to_string())?;
    }
    file.flush()
        .map_err(|_| "Could not save the executable patch".to_string())?;
    file.sync_all()
        .map_err(|_| "Could not finalize the executable patch".to_string())?;
    Ok(())
}

pub fn apply_patch_to_copy(
    source: &Path,
    destination: &Path,
    patch: &[u8],
    expected_source_size: u64,
) -> Result<(), String> {
    if destination.exists() {
        return Err("The temporary executable destination already exists".into());
    }
    fs::copy(source, destination)
        .map_err(|_| "Could not create a temporary copy of Wow.exe".to_string())?;
    if let Err(error) = apply_patch(destination, patch, expected_source_size) {
        fs::remove_file(destination).ok();
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_applies_fixed_length_patch() {
        let source = b"0123456789abcdef";
        let target = b"0123XY6789abZZef";
        let patch = build_patch(source, target).unwrap();
        assert_eq!(apply_patch_bytes(source, &patch).unwrap(), target);
        let hunks = parse_patch(&patch, source.len() as u64).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(
            hunks[0],
            PatchHunk {
                offset: 4,
                bytes: b"XY".to_vec()
            }
        );
        assert_eq!(
            hunks[1],
            PatchHunk {
                offset: 12,
                bytes: b"ZZ".to_vec()
            }
        );

        let dir = std::env::temp_dir().join(format!("rx-exe-patch-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("copy.exe");
        fs::write(&path, source).unwrap();
        apply_patch(&path, &patch, source.len() as u64).unwrap();
        assert_eq!(fs::read(&path).unwrap(), target);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_bad_ranges_and_trailing_bytes() {
        let hunks = vec![
            PatchHunk {
                offset: 4,
                bytes: b"x".to_vec(),
            },
            PatchHunk {
                offset: 4,
                bytes: b"y".to_vec(),
            },
        ];
        assert!(encode_patch(8, &hunks).is_err());

        let mut patch = build_patch(b"abcdefgh", b"abcdEfgh").unwrap();
        patch.push(0);
        assert!(parse_patch(&patch, 8).is_err());
    }

    #[test]
    fn rejects_size_changes() {
        assert!(build_patch(b"abc", b"abcd").is_err());
    }
}
