use super::*;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

pub async fn q_avatars(
    db: &DbCache,
    username: Option<String>,
    out: Option<String>,
    limit: Option<usize>,
) -> Result<Value> {
    let path = db
        .get("head_image/head_image.db")
        .await?
        .context("找不到 head_image.db，请确认微信数据目录")?;

    tokio::task::spawn_blocking(move || {
        let out_path = out.as_deref().map(Path::new);
        q_avatars_from_path(&path, username.as_deref(), out_path, limit)
    })
    .await?
}

fn q_avatars_from_path(
    path: &Path,
    username: Option<&str>,
    out: Option<&Path>,
    limit: Option<usize>,
) -> Result<Value> {
    let conn = Connection::open(path)?;
    let avatars = load_avatar_rows(&conn, username, limit)?;

    match out {
        Some(out_dir) => export_avatars(out_dir, &avatars),
        None => list_avatars(&avatars),
    }
}

struct AvatarRow {
    username: String,
    md5: String,
    image_buffer: Vec<u8>,
    update_time: i64,
}

fn load_avatar_rows(
    conn: &Connection,
    username: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<AvatarRow>> {
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(username) = username {
        clauses.push("username = ?");
        params.push(Box::new(username.to_string()));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let limit_clause = if let Some(limit) = limit {
        params.push(Box::new(limit as i64));
        " LIMIT ?"
    } else {
        ""
    };

    let sql = format!(
        "SELECT username, md5, image_buffer, update_time
         FROM head_image {} ORDER BY update_time DESC{}",
        where_clause, limit_clause
    );

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok(AvatarRow {
                username: row.get::<_, String>(0).unwrap_or_default(),
                md5: row.get::<_, String>(1).unwrap_or_default(),
                image_buffer: row.get::<_, Vec<u8>>(2)?,
                update_time: row.get::<_, i64>(3).unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

fn list_avatars(avatars: &[AvatarRow]) -> Result<Value> {
    let rows: Vec<Value> = avatars
        .iter()
        .map(|avatar| {
            json!({
                "username": avatar.username,
                "md5": avatar.md5,
                "image_size": avatar.image_buffer.len(),
                "update_time": avatar.update_time,
                "time": fmt_time(avatar.update_time, "%Y-%m-%d %H:%M"),
            })
        })
        .collect();

    Ok(json!({
        "count": rows.len(),
        "avatars": rows,
    }))
}

fn export_avatars(out_dir: &Path, avatars: &[AvatarRow]) -> Result<Value> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("创建头像导出目录失败: {}", out_dir.display()))?;

    let mut files = Vec::with_capacity(avatars.len());
    for avatar in avatars {
        let ext = avatar_image_ext(&avatar.image_buffer);
        let filename = format!("{}.{}", sanitize_avatar_username(&avatar.username), ext);
        let output_path = out_dir.join(filename);
        std::fs::write(&output_path, &avatar.image_buffer)
            .with_context(|| format!("写入头像文件失败: {}", output_path.display()))?;

        files.push(json!({
            "username": avatar.username,
            "md5": avatar.md5,
            "image_size": avatar.image_buffer.len(),
            "update_time": avatar.update_time,
            "time": fmt_time(avatar.update_time, "%Y-%m-%d %H:%M"),
            "ext": ext,
            "path": output_path.display().to_string(),
        }));
    }

    Ok(json!({
        "exported": files.len(),
        "out_dir": out_dir.display().to_string(),
        "files": files,
    }))
}

fn avatar_image_ext(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 3 && bytes[0..3] == [0xff, 0xd8, 0xff] {
        return "jpg";
    }
    if bytes.len() >= 4 && bytes[0..4] == [0x89, 0x50, 0x4e, 0x47] {
        return "png";
    }
    if bytes.len() >= 3 && bytes[0..3] == *b"GIF" {
        return "gif";
    }
    if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        return "webp";
    }
    "bin"
}

fn sanitize_avatar_username(input: &str) -> String {
    let without_parent = input.replace("..", "");
    let mut out = String::with_capacity(without_parent.len());
    for ch in without_parent.chars() {
        if ch == '/' || ch == '\\' {
            continue;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '@') {
            out.push(ch);
        }
    }

    if out.is_empty() {
        "avatar".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "wx-cli-avatars-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_head_image_db(tag: &str) -> PathBuf {
        let dir = unique_tmpdir(tag);
        let path = dir.join("head_image.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE head_image (
                username TEXT,
                md5 TEXT,
                image_buffer BLOB,
                update_time INTEGER
            )",
            [],
        )
        .unwrap();
        path
    }

    fn insert_avatar(
        conn: &Connection,
        username: &str,
        md5: &str,
        image_buffer: &[u8],
        update_time: i64,
    ) {
        conn.execute(
            "INSERT INTO head_image (username, md5, image_buffer, update_time)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![username, md5, image_buffer, update_time],
        )
        .unwrap();
    }

    #[test]
    fn lists_avatar_metadata_ordered_with_filter_and_limit() {
        let path = create_head_image_db("list");
        let conn = Connection::open(&path).unwrap();
        insert_avatar(
            &conn,
            "alice",
            "md5-old",
            &[0xff, 0xd8, 0xff, 1],
            1_700_000_000,
        );
        insert_avatar(
            &conn,
            "bob",
            "md5-new",
            &[0x89, 0x50, 0x4e, 0x47, 2, 3],
            1_800_000_000,
        );
        insert_avatar(&conn, "alice", "md5-mid", b"raw", 1_750_000_000);

        let value = q_avatars_from_path(&path, None, None, Some(2)).unwrap();
        let avatars = value["avatars"].as_array().unwrap();
        assert_eq!(value["count"], 2);
        assert_eq!(avatars[0]["username"], "bob");
        assert_eq!(avatars[0]["md5"], "md5-new");
        assert_eq!(avatars[0]["image_size"], 6);
        assert_eq!(avatars[0]["update_time"], 1_800_000_000);
        assert!(avatars[0]["time"].as_str().unwrap().contains("2027-01-15"));
        assert_eq!(avatars[1]["username"], "alice");
        assert_eq!(avatars[1]["md5"], "md5-mid");

        let filtered = q_avatars_from_path(&path, Some("alice"), None, None).unwrap();
        let filtered_avatars = filtered["avatars"].as_array().unwrap();
        assert_eq!(filtered["count"], 2);
        assert!(filtered_avatars
            .iter()
            .all(|avatar| avatar["username"] == "alice"));
    }

    #[test]
    fn detects_avatar_image_extensions_from_magic_bytes() {
        assert_eq!(avatar_image_ext(&[0xff, 0xd8, 0xff, 0x00]), "jpg");
        assert_eq!(avatar_image_ext(&[0x89, 0x50, 0x4e, 0x47, 0x00]), "png");
        assert_eq!(avatar_image_ext(b"GIF89a"), "gif");
        assert_eq!(avatar_image_ext(b"RIFF1234WEBPxxxx"), "webp");
        assert_eq!(avatar_image_ext(b"unknown"), "bin");
    }

    #[test]
    fn exports_avatar_files_with_magic_extensions() {
        let path = create_head_image_db("export");
        let conn = Connection::open(&path).unwrap();
        insert_avatar(&conn, "alice", "jpg-md5", &[0xff, 0xd8, 0xff, 0xaa], 20);
        insert_avatar(&conn, "bob", "webp-md5", b"RIFF1234WEBPpayload", 30);
        let out = unique_tmpdir("export-out");

        let value = q_avatars_from_path(&path, None, Some(&out), None).unwrap();
        assert_eq!(value["exported"], 2);
        assert_eq!(value["out_dir"], out.display().to_string());
        let files = value["files"].as_array().unwrap();
        assert_eq!(files[0]["username"], "bob");
        assert_eq!(files[0]["ext"], "webp");
        assert!(out.join("bob.webp").exists());
        assert_eq!(
            std::fs::read(out.join("bob.webp")).unwrap(),
            b"RIFF1234WEBPpayload"
        );
        assert!(out.join("alice.jpg").exists());
    }

    #[test]
    fn export_mode_filters_username_and_sanitizes_paths() {
        let path = create_head_image_db("filter-sanitize");
        let conn = Connection::open(&path).unwrap();
        insert_avatar(
            &conn,
            "../evil/../../wxid",
            "png-md5",
            &[0x89, 0x50, 0x4e, 0x47],
            40,
        );
        insert_avatar(&conn, "other", "gif-md5", b"GIF87a", 50);
        let out = unique_tmpdir("sanitize-out");

        let value =
            q_avatars_from_path(&path, Some("../evil/../../wxid"), Some(&out), None).unwrap();
        assert_eq!(value["exported"], 1);
        let files = value["files"].as_array().unwrap();
        assert_eq!(files[0]["username"], "../evil/../../wxid");
        assert_eq!(
            files[0]["path"],
            out.join("evilwxid.png").display().to_string()
        );
        assert!(out.join("evilwxid.png").exists());
        assert!(!out.join("other.gif").exists());
        assert_eq!(sanitize_avatar_username("../../"), "avatar");
    }
}
