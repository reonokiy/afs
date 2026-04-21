use sqlx::SqlitePool;

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await?;

    // Enable WAL mode for better concurrent read performance
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn open_db(path: &str) -> Result<SqlitePool, sqlx::Error> {
    let url = format!("sqlite:{}?mode=rwc", path);
    let pool = SqlitePool::connect(&url).await?;
    run_migrations(&pool).await?;
    Ok(pool)
}
