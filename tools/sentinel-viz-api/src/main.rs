use anyhow::Result;
use sentinel_viz_api::db;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let path = db::default_db_path()?;
    let conn = db::open_ro(&path)?;
    let events = db::read_events(&conn)?;
    tracing::info!(path = %path.display(), count = events.len(), "read events");
    println!("sentinel-viz-api: read {} events from {}", events.len(), path.display());
    Ok(())
}
