//! End-to-end smoke check against a live gateway, using the saved sign-in.
//!
//! ```bash
//! cargo run -p bench-core --example smoke
//! ```
//!
//! Verifies the things unit tests structurally cannot: that the MCP handshake
//! completes, that a chain compiled locally is accepted by `flow.into`, and
//! that per-step results come back in the shape the parser expects. Costs a
//! small number of credits.

use std::sync::Arc;

use serde_json::Value;

use bench_core::{
    auth,
    chain::Chain,
    dg::ConduitClient,
    features::Features,
    session::Session,
    source::{synth::SynthSource, Source},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = auth::credentials_path();
    let Some(credentials) = auth::load(&path)? else {
        eprintln!(
            "No saved sign-in at {}. Run Bench and click Connect.",
            path.display()
        );
        std::process::exit(1);
    };
    println!("gateway: {}", credentials.gateway);

    // Persist rotated grants: DataGrout rotates refresh tokens, and a process
    // that refreshes without writing back leaves a consumed token on disk.
    //
    // ONE client for the whole run. Two clients built from the same grant is
    // the rotation race in miniature: the first to refresh consumes the
    // refresh token and the second then presents a dead one, which the
    // gateway answers with `invalid_grant` — "sign-in expired" for a sign-in
    // that is perfectly fine on disk.
    let persist_path = path.clone();
    let client = Arc::new(
        ConduitClient::connect(&credentials.gateway, credentials.grant.clone())?
            .with_refresh_handler(move |grant| {
                if let Err(e) = auth::save_refreshed_grant(&persist_path, grant) {
                    eprintln!("warning: could not persist refreshed grant: {e}");
                } else {
                    println!("(grant refreshed and saved)");
                }
            }),
    );
    let session = Session::new(client.clone());

    // A known-truth frame: 440 Hz plus a third harmonic.
    let mut source = SynthSource::audio(48_000.0);
    let mut samples = Vec::new();
    let frame: usize = std::env::var("BENCH_FRAME")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(bench_core::CAPTURE_SAMPLES);
    source.fill(&mut samples, frame);
    println!("frame: {frame} samples");

    // `BENCH_CHAIN="signal.filter,signal.convolve"` overrides the default
    // single-step chain, to probe how multi-step bindings behave live.
    let mut chain = Chain::new();
    let keys = std::env::var("BENCH_CHAIN").unwrap_or_else(|_| "signal.spectral".to_string());
    for key in keys.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        chain.push(key)?;
    }
    println!(
        "\nrunning chain: {:?}",
        chain.steps().iter().map(|s| &s.key).collect::<Vec<_>>()
    );

    // Raw envelope first, so a parsing failure is distinguishable from a
    // gateway failure.
    {
        use bench_core::dg::{tools, DgClient};
        let args = chain.to_flow_args(&samples, bench_core::chain::Encoding::Base64F32);
        let raw = client.call_tool(tools::FLOW_INTO, args).await?;
        let pretty = serde_json::to_string_pretty(&raw).unwrap_or_default();
        println!("raw envelope: {} bytes", pretty.len());
        println!(
            "top-level keys: {:?}",
            raw.as_object().map(|o| {
                let mut k: Vec<_> = o.keys().cloned().collect();
                k.sort();
                k
            })
        );
        if let Some(hint) = raw.get("_hint").and_then(|h| h.as_str()) {
            println!("TRUNCATED: {}", &hint[..hint.len().min(160)]);
        }
        if let Some(cr) = raw.get("cache_ref").and_then(|c| c.as_str()) {
            println!("cache_ref: {cr}");
        }
        let has_steps = raw.pointer("/execution_result/step_outputs").is_some();
        let has_results = raw.pointer("/execution_result/results").is_some();
        println!("step_outputs present: {has_steps}   results present: {has_results}");
        for key in ["status", "error", "errors", "failed_step", "message"] {
            if let Some(v) = raw
                .pointer(&format!("/execution_result/{key}"))
                .or_else(|| raw.get(key))
            {
                println!("{key}: {}", serde_json::to_string(v).unwrap_or_default());
            }
        }
        if let Some(results) = raw
            .pointer("/execution_result/results")
            .and_then(Value::as_object)
        {
            for (name, value) in results {
                let summary = match value {
                    Value::Object(o) => format!(
                        "object keys {:?}",
                        o.iter()
                            .map(|(k, v)| match v {
                                Value::Array(a) => format!("{k}[{}]", a.len()),
                                Value::Object(_) => format!("{k}{{}}"),
                                other => format!(
                                    "{k}={}",
                                    serde_json::to_string(other)
                                        .unwrap_or_default()
                                        .chars()
                                        .take(60)
                                        .collect::<String>()
                                ),
                            })
                            .collect::<Vec<_>>()
                    ),
                    Value::Array(a) => format!("array[{}]", a.len()),
                    other => serde_json::to_string(other)
                        .unwrap_or_default()
                        .chars()
                        .take(300)
                        .collect(),
                };
                println!("  {name}: {summary}");
            }
        }
        if std::env::var_os("BENCH_RAW").is_some() {
            println!("{}", pretty.chars().take(6000).collect::<String>());
        }

        // A headed envelope hides the per-step results; fetch the cached full
        // one and summarise its results the same way.
        if let Some(cache_ref) = raw.get("cache_ref").and_then(Value::as_str) {
            let page = client
                .call_tool(
                    tools::PRISM_PAGINATE,
                    serde_json::json!({ "cache_ref": cache_ref, "page": 1, "per_page": 1 }),
                )
                .await?;
            let full = page.pointer("/records/0").cloned().unwrap_or(Value::Null);
            println!(
                "\nfull envelope via cache: top-level keys {:?}",
                full.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
            for key in [
                "status",
                "error",
                "errors",
                "failed_step",
                "message",
                "steps_completed",
            ] {
                if let Some(v) = full
                    .pointer(&format!("/execution_result/{key}"))
                    .or_else(|| full.get(key))
                {
                    println!(
                        "{key}: {}",
                        serde_json::to_string(v)
                            .unwrap_or_default()
                            .chars()
                            .take(600)
                            .collect::<String>()
                    );
                }
            }
            if let Some(results) = full
                .pointer("/execution_result/results")
                .and_then(Value::as_object)
            {
                for (name, value) in results {
                    let summary = match value {
                        Value::Object(o) => o
                            .iter()
                            .map(|(k, v)| match v {
                                Value::Array(a) => format!("{k}[{}]", a.len()),
                                Value::Object(_) => format!("{k}{{}}"),
                                other => format!(
                                    "{k}={}",
                                    serde_json::to_string(other)
                                        .unwrap_or_default()
                                        .chars()
                                        .take(120)
                                        .collect::<String>()
                                ),
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        Value::Array(a) => format!("array[{}]", a.len()),
                        other => serde_json::to_string(other)
                            .unwrap_or_default()
                            .chars()
                            .take(300)
                            .collect(),
                    };
                    println!("  {name}: {summary}");
                }
            }
            if let Some(outputs) = full
                .pointer("/execution_result/step_outputs")
                .and_then(Value::as_array)
            {
                for (i, o) in outputs.iter().enumerate() {
                    println!(
                        "  step_outputs[{i}] keys {:?}",
                        o.as_object().map(|m| m.keys().collect::<Vec<_>>())
                    );
                    if let Some(err) = o.get("error") {
                        println!(
                            "    error: {}",
                            serde_json::to_string(err)
                                .unwrap_or_default()
                                .chars()
                                .take(600)
                                .collect::<String>()
                        );
                    }
                }
            }
        }
    }

    let run = session.run_chain(&chain, &samples).await?;

    for (i, step) in run.steps.iter().enumerate() {
        match step {
            Some(result) => println!("  step {}: {result:?}", i + 1),
            None => println!("  step {}: (no result)", i + 1),
        }
    }
    if let Some(ctc) = &run.ctc_id {
        println!("  certificate: {ctc}");
    }
    println!(
        "  fetched from cache: {} ({})",
        run.fetched_from_cache,
        if run.fetched_from_cache {
            "oversized → prism.paginate, +1 credit"
        } else {
            "inline"
        }
    );

    // The whole point of the spectral step: does it find the 440 Hz tone?
    match run.spectral() {
        Some((dominant, thd)) => {
            println!("\ndominant: {dominant:?} Hz   THD: {thd:?} %");
            match dominant {
                Some(hz) if (hz - 440.0).abs() <= source.sample_rate() / frame as f64 => {
                    println!(
                        "PASS — found the fundamental within one bin ({:.1} Hz/bin)",
                        source.sample_rate() / frame as f64
                    )
                }
                Some(hz) => println!("UNEXPECTED — dominant {hz} Hz, expected ~440"),
                None => println!("UNEXPECTED — no dominant frequency reported"),
            }
        }
        None => println!("\nno spectral step in the results"),
    }

    let features = Features::from_samples("smoke_capture", &samples, source.sample_rate());
    println!(
        "\nlocal measurements: rms={:?} pk-pk={:?}",
        features.rms, features.peak_to_peak
    );

    Ok(())
}
