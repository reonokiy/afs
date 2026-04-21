use sqlx::SqlitePool;

use crate::{BaseNode, NodeKind, OverlayKind, OverlayNode};

// ── base_nodes ──────────────────────────────────────────────────

/// Insert a batch of base nodes for a new generation.
pub async fn publish_generation(
    pool: &SqlitePool,
    generation: i64,
    nodes: &[BaseNode],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query!("DELETE FROM base_nodes WHERE gen = ?", generation)
        .execute(&mut *tx)
        .await?;

    for node in nodes {
        let kind = node.kind as i32;
        sqlx::query!(
            "INSERT INTO base_nodes (gen, path, parent, kind, oid, mode, size)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            generation,
            node.path,
            node.parent,
            kind,
            node.oid,
            node.mode,
            node.size,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Get a single base node by generation and path.
pub async fn get_base_node(
    pool: &SqlitePool,
    generation: i64,
    path: &str,
) -> Result<Option<BaseNode>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT path as "path!", parent as "parent!", kind, oid, mode, size
         FROM base_nodes WHERE gen = ? AND path = ?"#,
        generation,
        path,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| BaseNode {
        generation,
        path: r.path,
        parent: r.parent,
        kind: NodeKind::try_from(r.kind as i32).unwrap_or(NodeKind::Blob),
        oid: r.oid,
        mode: r.mode,
        size: r.size,
    }))
}

/// List direct children of a parent path in a given generation.
pub async fn list_children(
    pool: &SqlitePool,
    generation: i64,
    parent: &str,
) -> Result<Vec<BaseNode>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT path as "path!", parent as "parent!", kind, oid, mode, size
         FROM base_nodes WHERE gen = ? AND parent = ? ORDER BY path"#,
        generation,
        parent,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| BaseNode {
            generation,
            path: r.path,
            parent: r.parent,
            kind: NodeKind::try_from(r.kind as i32).unwrap_or(NodeKind::Blob),
            oid: r.oid,
            mode: r.mode,
            size: r.size,
        })
        .collect())
}

/// Update the size of a blob after hydration.
pub async fn update_blob_size(
    pool: &SqlitePool,
    generation: i64,
    oid: &str,
    size: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE base_nodes SET size = ? WHERE gen = ? AND oid = ?",
        size,
        generation,
        oid,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Clean up old generations, keeping only the most recent two.
pub async fn prune_old_generations(pool: &SqlitePool, current_gen: i64) -> Result<(), sqlx::Error> {
    if current_gen > 2 {
        let cutoff = current_gen - 1;
        sqlx::query!("DELETE FROM base_nodes WHERE gen < ?", cutoff)
            .execute(pool)
            .await?;
    }
    Ok(())
}

// ── overlay_nodes ───────────────────────────────────────────────

/// Get an overlay node by path.
pub async fn get_overlay_node(
    pool: &SqlitePool,
    path: &str,
) -> Result<Option<OverlayNode>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT path as "path!", kind as "kind!", backing, mode, size, mtime_ns, source_oid
         FROM overlay_nodes WHERE path = ?"#,
        path,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| OverlayNode {
        path: r.path,
        kind: r.kind.parse::<OverlayKind>().unwrap_or(OverlayKind::Create),
        backing: r.backing,
        mode: r.mode,
        size: r.size,
        mtime_ns: r.mtime_ns,
        source_oid: r.source_oid,
    }))
}

/// Upsert an overlay node.
pub async fn upsert_overlay_node(
    pool: &SqlitePool,
    node: &OverlayNode,
) -> Result<(), sqlx::Error> {
    let kind = node.kind.as_str();
    sqlx::query!(
        "INSERT INTO overlay_nodes (path, kind, backing, mode, size, mtime_ns, source_oid)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(path) DO UPDATE SET
           kind = excluded.kind,
           backing = excluded.backing,
           mode = excluded.mode,
           size = excluded.size,
           mtime_ns = excluded.mtime_ns,
           source_oid = excluded.source_oid",
        node.path,
        kind,
        node.backing,
        node.mode,
        node.size,
        node.mtime_ns,
        node.source_oid,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete an overlay node by path (hard delete from table).
pub async fn delete_overlay_node(pool: &SqlitePool, path: &str) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM overlay_nodes WHERE path = ?", path)
        .execute(pool)
        .await?;
    Ok(())
}

/// List overlay entries whose path starts with a given prefix (direct children).
pub async fn list_overlay_by_prefix(
    pool: &SqlitePool,
    prefix: &str,
) -> Result<Vec<OverlayNode>, sqlx::Error> {
    let pattern = if prefix == "." {
        "%".to_string()
    } else {
        format!("{}/%", prefix)
    };

    let rows = sqlx::query!(
        r#"SELECT path as "path!", kind as "kind!", backing, mode, size, mtime_ns, source_oid
         FROM overlay_nodes WHERE path LIKE ? ORDER BY path"#,
        pattern,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| OverlayNode {
            path: r.path,
            kind: r.kind.parse::<OverlayKind>().unwrap_or(OverlayKind::Create),
            backing: r.backing,
            mode: r.mode,
            size: r.size,
            mtime_ns: r.mtime_ns,
            source_oid: r.source_oid,
        })
        .collect())
}

/// Count non-deleted overlay entries (dirty count).
pub async fn overlay_dirty_count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row = sqlx::query!("SELECT COUNT(*) as count FROM overlay_nodes WHERE kind <> 'delete'")
        .fetch_one(pool)
        .await?;
    Ok(row.count)
}
