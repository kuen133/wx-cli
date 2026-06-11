use super::*;

pub(super) fn image_cache_candidates(
    account_root: &Path,
    table: &str,
    local_id: i64,
    create_time: i64,
) -> Vec<PathBuf> {
    let Some(session_hash) = table.strip_prefix("Msg_") else {
        return Vec::new();
    };
    if session_hash.len() != 32 || !session_hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Vec::new();
    }

    let month = fmt_time(create_time, "%Y-%m");
    vec![account_root.join(format!(
        "cache/{}/Message/{}/Thumb/{}_{}_thumb.jpg",
        month, session_hash, local_id, create_time
    ))]
}

fn bubble_cache_candidate(
    account_root: &Path,
    table: &str,
    local_id: i64,
    create_time: i64,
) -> Option<PathBuf> {
    let session_hash = table.strip_prefix("Msg_")?;
    if session_hash.len() != 32 || !session_hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }

    let month = fmt_time(create_time, "%Y-%m");
    Some(account_root.join(format!(
        "cache/{}/Message/{}/Bubble/{}_{}_b.dat",
        month, session_hash, local_id, create_time
    )))
}

pub(super) fn existing_image_paths(
    account_root: &Path,
    table: &str,
    local_id: i64,
    create_time: i64,
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> =
        image_cache_candidates(account_root, table, local_id, create_time)
            .into_iter()
            .filter(|path| path.is_file())
            .collect();

    if let Some(bubble_path) = bubble_cache_candidate(account_root, table, local_id, create_time) {
        if let Some(extracted) =
            materialize_embedded_image(table, local_id, create_time, &bubble_path)
        {
            paths.push(extracted);
        }
    }

    paths
}

pub(super) fn materialize_embedded_image(
    table: &str,
    local_id: i64,
    create_time: i64,
    source: &Path,
) -> Option<PathBuf> {
    if !source.is_file() {
        return None;
    }

    let session_hash = table.strip_prefix("Msg_")?;
    let bytes = std::fs::read(source).ok()?;
    let image = extract_embedded_image_bytes(&bytes)?;
    let extension = embedded_image_extension(&image)?;
    let output_dir = config::cache_dir().join("image_extract").join(session_hash);
    std::fs::create_dir_all(&output_dir).ok()?;
    let output = output_dir.join(format!("{}_{}{}", local_id, create_time, extension));
    if output.is_file() {
        return Some(output);
    }
    std::fs::write(&output, image).ok()?;
    Some(output)
}

pub(super) fn extract_embedded_image_bytes(data: &[u8]) -> Option<Vec<u8>> {
    for signature in [
        &[0xff, 0xd8, 0xff][..],
        &[0x89, b'P', b'N', b'G'][..],
        b"GIF8".as_slice(),
    ] {
        if let Some(offset) = find_bytes(data, signature) {
            return Some(data[offset..].to_vec());
        }
    }
    None
}

fn embedded_image_extension(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(".jpg");
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some(".png");
    }
    if data.starts_with(b"GIF8") {
        return Some(".gif");
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
