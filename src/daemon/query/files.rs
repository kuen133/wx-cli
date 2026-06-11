use super::*;
use rusqlite::OptionalExtension;
use std::path::Path;

const HARDLINK_DB_KEY: &str = "hardlink/hardlink.db";

pub async fn q_files(
    db: &DbCache,
    file_type: Option<String>,
    limit: Option<usize>,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let path = db
        .get(HARDLINK_DB_KEY)
        .await?
        .context("找不到 hardlink.db，请确认微信数据目录")?;

    tokio::task::spawn_blocking(move || {
        q_files_from_path(&path, file_type.as_deref(), limit, since, until)
    })
    .await?
}

#[derive(Clone, Copy)]
enum FileKind {
    Image,
    Video,
    File,
}

impl FileKind {
    fn all() -> [Self; 3] {
        [Self::Image, Self::Video, Self::File]
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "image" => Ok(Self::Image),
            "video" => Ok(Self::Video),
            "file" => Ok(Self::File),
            _ => anyhow::bail!("未知文件类型: {}", s),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::File => "file",
        }
    }

    fn table(self) -> &'static str {
        match self {
            Self::Image => "image_hardlink_info_v4",
            Self::Video => "video_hardlink_info_v4",
            Self::File => "file_hardlink_info_v4",
        }
    }
}

struct FileRow {
    kind: FileKind,
    md5: String,
    file_name: String,
    file_size: i64,
    modify_time: i64,
}

fn q_files_from_path(
    path: &Path,
    file_type: Option<&str>,
    limit: Option<usize>,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Value> {
    let conn = Connection::open(path)?;
    let kinds: Vec<FileKind> = match file_type {
        Some(s) => vec![FileKind::parse(s)?],
        None => FileKind::all().to_vec(),
    };

    let mut files = Vec::new();
    for kind in kinds {
        if !hardlink_table_exists(&conn, kind.table())? {
            continue;
        }
        files.extend(load_files_for_kind(&conn, kind, since, until)?);
    }

    files.sort_by(|a, b| b.modify_time.cmp(&a.modify_time));
    if let Some(limit) = limit {
        files.truncate(limit);
    }

    list_files(&files)
}

fn hardlink_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn load_files_for_kind(
    conn: &Connection,
    kind: FileKind,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Vec<FileRow>> {
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(since) = since {
        clauses.push("modify_time >= ?");
        params.push(Box::new(since));
    }
    if let Some(until) = until {
        clauses.push("modify_time <= ?");
        params.push(Box::new(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT md5, file_name, file_size, modify_time
         FROM {} {} ORDER BY modify_time DESC",
        kind.table(),
        where_clause
    );

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok(FileRow {
                kind,
                md5: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                file_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                file_size: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                modify_time: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

fn list_files(files: &[FileRow]) -> Result<Value> {
    let rows: Vec<Value> = files
        .iter()
        .map(|file| {
            json!({
                "kind": file.kind.as_str(),
                "md5": file.md5,
                "file_name": file.file_name,
                "file_size": file.file_size,
                "modify_time": file.modify_time,
                "time": fmt_time(file.modify_time, "%Y-%m-%d %H:%M"),
            })
        })
        .collect();

    Ok(json!({
        "count": rows.len(),
        "files": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "wx-cli-files-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_hardlink_db(tag: &str) -> PathBuf {
        let dir = unique_tmpdir(tag);
        let path = dir.join("hardlink.db");
        let conn = Connection::open(&path).unwrap();
        for table in [
            "image_hardlink_info_v4",
            "video_hardlink_info_v4",
            "file_hardlink_info_v4",
        ] {
            create_hardlink_table(&conn, table);
        }
        path
    }

    fn create_hardlink_table(conn: &Connection, table: &str) {
        conn.execute(
            &format!(
                "CREATE TABLE {table} (
                    md5_hash INTEGER,
                    md5 TEXT,
                    type INTEGER,
                    file_name TEXT,
                    file_size INTEGER,
                    modify_time INTEGER,
                    dir1 INTEGER,
                    dir2 INTEGER,
                    extra_buffer BLOB
                )"
            ),
            [],
        )
        .unwrap();
    }

    fn insert_file(
        conn: &Connection,
        table: &str,
        md5: &str,
        file_name: &str,
        file_size: Option<i64>,
        modify_time: Option<i64>,
    ) {
        conn.execute(
            &format!(
                "INSERT INTO {table}
                 (md5_hash, md5, type, file_name, file_size, modify_time, dir1, dir2, extra_buffer)
                 VALUES (0, ?1, 0, ?2, ?3, ?4, 0, 0, X'')"
            ),
            rusqlite::params![md5, file_name, file_size, modify_time],
        )
        .unwrap();
    }

    #[test]
    fn filters_by_type_and_handles_null_size_and_time() {
        let path = create_hardlink_db("kind");
        let conn = Connection::open(&path).unwrap();
        insert_file(
            &conn,
            "image_hardlink_info_v4",
            "md5-image",
            "photo.jpg",
            None,
            None,
        );
        insert_file(
            &conn,
            "video_hardlink_info_v4",
            "md5-video",
            "clip.mp4",
            Some(200),
            Some(2000),
        );

        let value = q_files_from_path(&path, Some("image"), None, None, None).unwrap();
        assert_eq!(value["count"], 1);
        let files = value["files"].as_array().unwrap();
        assert_eq!(files[0]["kind"], "image");
        assert_eq!(files[0]["md5"], "md5-image");
        assert_eq!(files[0]["file_name"], "photo.jpg");
        assert_eq!(files[0]["file_size"], 0);
        assert_eq!(files[0]["modify_time"], 0);
    }

    #[test]
    fn merges_tables_orders_globally_and_applies_limit() {
        let path = create_hardlink_db("merge");
        let conn = Connection::open(&path).unwrap();
        insert_file(
            &conn,
            "image_hardlink_info_v4",
            "md5-old",
            "old.jpg",
            Some(10),
            Some(1000),
        );
        insert_file(
            &conn,
            "video_hardlink_info_v4",
            "md5-new",
            "new.mp4",
            Some(20),
            Some(3000),
        );
        insert_file(
            &conn,
            "file_hardlink_info_v4",
            "md5-mid",
            "mid.pdf",
            Some(30),
            Some(2000),
        );

        let value = q_files_from_path(&path, None, Some(2), None, None).unwrap();
        assert_eq!(value["count"], 2);
        let files = value["files"].as_array().unwrap();
        assert_eq!(files[0]["kind"], "video");
        assert_eq!(files[0]["md5"], "md5-new");
        assert_eq!(files[1]["kind"], "file");
        assert_eq!(files[1]["md5"], "md5-mid");
    }

    #[test]
    fn filters_by_modify_time_range() {
        let path = create_hardlink_db("time");
        let conn = Connection::open(&path).unwrap();
        insert_file(
            &conn,
            "image_hardlink_info_v4",
            "md5-before",
            "before.jpg",
            Some(10),
            Some(1000),
        );
        insert_file(
            &conn,
            "video_hardlink_info_v4",
            "md5-inside",
            "inside.mp4",
            Some(20),
            Some(2000),
        );
        insert_file(
            &conn,
            "file_hardlink_info_v4",
            "md5-after",
            "after.pdf",
            Some(30),
            Some(3000),
        );

        let value = q_files_from_path(&path, None, None, Some(1500), Some(2500)).unwrap();
        assert_eq!(value["count"], 1);
        let files = value["files"].as_array().unwrap();
        assert_eq!(files[0]["md5"], "md5-inside");
        assert_eq!(files[0]["modify_time"], 2000);
    }

    #[test]
    fn skips_missing_hardlink_tables() {
        let dir = unique_tmpdir("missing-table");
        let path = dir.join("hardlink.db");
        let conn = Connection::open(&path).unwrap();
        create_hardlink_table(&conn, "file_hardlink_info_v4");
        insert_file(
            &conn,
            "file_hardlink_info_v4",
            "md5-file",
            "doc.pdf",
            Some(42),
            Some(4000),
        );

        let value = q_files_from_path(&path, None, None, None, None).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["files"][0]["kind"], "file");

        let missing_kind = q_files_from_path(&path, Some("image"), None, None, None).unwrap();
        assert_eq!(missing_kind["count"], 0);
        assert!(missing_kind["files"].as_array().unwrap().is_empty());
    }
}
