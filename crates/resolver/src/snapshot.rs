use afs_db::BaseNode;
use anyhow::Result;
use sqlx::SqlitePool;

/// Query a single node by generation + path.
pub async fn get_node(pool: &SqlitePool, generation: i64, path: &str) -> Result<Option<BaseNode>> {
    Ok(afs_db::nodes::get_base_node(pool, generation, path).await?)
}

/// List direct children of a directory.
pub async fn list_children(pool: &SqlitePool, generation: i64, parent: &str) -> Result<Vec<BaseNode>> {
    Ok(afs_db::nodes::list_children(pool, generation, parent).await?)
}
