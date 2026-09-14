//! Load the signed-in account's Smart Panels from a live gateway.
//!
//! ```bash
//! cargo run -p bench-core --example panels            # list only: one call
//! cargo run -p bench-core --example panels -- rows    # also load full rows
//! ```
//!
//! Checks what unit tests cannot: that `smart-panels.list` answers in the shape
//! the panels crate folds into dashboards, and — with `rows` — that the
//! `panel_data` snapshot and `panel_source` fallback yield rows for real
//! panels. Listing is one call; `rows` is one `logic.query` per data panel.

use std::sync::Arc;

use bench_core::{auth, dg::ConduitClient, session::Session};
use datagrout_panels::Panel;

fn describe(panel: &Panel, depth: usize) {
    let pad = "  ".repeat(depth);
    println!(
        "{pad}{} [{}] ns={} title={:?} rows={} fields={} children={} published={}",
        panel.id,
        panel.kind.as_str(),
        panel.namespace,
        panel.title(),
        panel.rows.len(),
        panel.fields.len(),
        panel.children.len(),
        panel.published,
    );
    let columns = panel.columns();
    if !columns.is_empty() {
        println!("{pad}  columns: {columns:?}");
    }
    for row in panel.rows.iter().take(3) {
        println!("{pad}  {row:?}");
    }
    if panel.rows.len() > 3 {
        println!("{pad}  … {} more", panel.rows.len() - 3);
    }
    for child in &panel.children {
        describe(child, depth + 1);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let want_rows = std::env::args().any(|a| a == "rows");

    let path = auth::credentials_path();
    let Some(credentials) = auth::load(&path)? else {
        eprintln!(
            "No saved sign-in at {}. Run Bench and click Connect.",
            path.display()
        );
        std::process::exit(1);
    };
    println!("gateway: {}", credentials.gateway);

    let persist_path = path.clone();
    let client = ConduitClient::connect(&credentials.gateway, credentials.grant.clone())?
        .with_refresh_handler(move |grant| {
            if let Err(e) = auth::save_refreshed_grant(&persist_path, grant) {
                eprintln!("warning: could not persist refreshed grant: {e}");
            }
        });
    let session = Session::new(Arc::new(client));

    let panels = session.list_panels().await?;
    println!("\n{} top-level panel(s):\n", panels.len());
    for panel in &panels {
        describe(panel, 0);
    }

    if want_rows {
        for panel in &panels {
            let calls = Session::row_load_calls(panel);
            println!("\nloading rows for {} ({calls} query call(s))…", panel.id);
            let loaded = session.load_panel_rows(panel).await?;
            describe(&loaded, 0);
        }
    } else {
        let total: usize = panels.iter().map(Session::row_load_calls).sum();
        println!("\n(pass `rows` to load full rows: {total} logic.query call(s) in total)");
    }

    Ok(())
}
