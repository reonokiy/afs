use sqlx::SqlitePool;

// ── pack_index ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PackEntry {
    pub oid: String,
    pub pack_id: String,
    pub offset: i64,
    pub comp_size: i64,
    pub raw_size: i64,
}

/// Look up a blob's location within a pack file.
pub async fn get_pack_entry(
    pool: &SqlitePool,
    oid: &str,
) -> Result<Option<PackEntry>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT oid as "oid!", pack_id as "pack_id!", offset, comp_size, raw_size
         FROM pack_index WHERE oid = ?"#,
        oid,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| PackEntry {
        oid: r.oid,
        pack_id: r.pack_id,
        offset: r.offset,
        comp_size: r.comp_size,
        raw_size: r.raw_size,
    }))
}

/// Bulk insert pack entries (used when syncing manifest from S3).
pub async fn bulk_insert_pack_entries(
    pool: &SqlitePool,
    entries: &[PackEntry],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    for entry in entries {
        sqlx::query!(
            "INSERT OR REPLACE INTO pack_index (oid, pack_id, offset, comp_size, raw_size)
             VALUES (?, ?, ?, ?, ?)",
            entry.oid,
            entry.pack_id,
            entry.offset,
            entry.comp_size,
            entry.raw_size,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Look up multiple blobs' locations within pack files in a single query.
/// Uses a dynamically built `WHERE oid IN (?, ?, ...)` clause.
pub async fn get_pack_entries_batch(
    pool: &SqlitePool,
    oids: &[&str],
) -> Result<Vec<PackEntry>, sqlx::Error> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }

    // SQLite has a default SQLITE_MAX_VARIABLE_NUMBER of 999.
    // Process in chunks to stay within that limit.
    let mut results = Vec::new();
    for chunk in oids.chunks(500) {
        let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query_str = format!(
            "SELECT oid, pack_id, offset, comp_size, raw_size FROM pack_index WHERE oid IN ({})",
            placeholders
        );

        let mut query = sqlx::query_as::<_, (String, String, i64, i64, i64)>(&query_str);
        for oid in chunk {
            query = query.bind(*oid);
        }

        let rows = query.fetch_all(pool).await?;
        for (oid, pack_id, offset, comp_size, raw_size) in rows {
            results.push(PackEntry {
                oid,
                pack_id,
                offset,
                comp_size,
                raw_size,
            });
        }
    }

    Ok(results)
}

/// Clear all pack index entries (used before full manifest re-sync).
pub async fn clear_pack_index(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM pack_index")
        .execute(pool)
        .await?;
    Ok(())
}

// ── sync_state ──────────────────────────────────────────────────

/// Get a sync state value by key.
pub async fn get_sync_state(
    pool: &SqlitePool,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT value as "value!" FROM sync_state WHERE key = ?"#,
        key,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.value))
}

/// Set a sync state value.
pub async fn set_sync_state(
    pool: &SqlitePool,
    key: &str,
    value: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO sync_state (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        key,
        value,
    )
    .execute(pool)
    .await?;
    Ok(())
}
