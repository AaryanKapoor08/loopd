//! `loop policy [--on-trip … --max-cost … --max-iterations … --max-duration …]`
//!
//! Sugar over `loop set` for the governance knobs. With no flags it prints the
//! current policy; with flags it edits the matching `defaults.*` config keys
//! (validated + persisted via [`super::set::edit_config`]) and echoes the result.

use anyhow::Result;
use clap::Args;

use crate::config::Config;

/// Arguments for `loop policy`. All optional — none set means "show".
#[derive(Args, Debug)]
pub struct PolicyArgs {
    /// On-trip action: `warn` | `notify` | `pause` | `kill`.
    #[arg(long)]
    pub on_trip: Option<String>,
    /// Max cumulative cost in USD before a cap trips.
    #[arg(long)]
    pub max_cost: Option<f64>,
    /// Max agent iterations before a cap trips.
    #[arg(long)]
    pub max_iterations: Option<u32>,
    /// Max wall-clock minutes before a cap trips.
    #[arg(long)]
    pub max_duration: Option<u32>,
}

pub fn policy(args: PolicyArgs) -> Result<()> {
    let mut updates: Vec<(String, String)> = Vec::new();
    if let Some(v) = &args.on_trip {
        updates.push(("defaults.onTrip".into(), v.clone()));
    }
    if let Some(v) = args.max_cost {
        updates.push(("defaults.caps.maxCostUsd".into(), v.to_string()));
    }
    if let Some(v) = args.max_iterations {
        updates.push(("defaults.caps.maxIterations".into(), v.to_string()));
    }
    if let Some(v) = args.max_duration {
        updates.push(("defaults.caps.maxDurationMin".into(), v.to_string()));
    }

    let config = if updates.is_empty() {
        Config::load()?
    } else {
        let updated = super::set::edit_config(&updates)?;
        println!("updated policy:");
        updated
    };

    print_policy(&config);
    if !updates.is_empty() {
        println!("\n{}", super::set::apply_note());
    }
    Ok(())
}

fn print_policy(config: &Config) {
    let d = &config.defaults;
    println!("on-trip:        {:?}", d.on_trip);
    println!("caps:");
    println!("  max cost:     ${:.2}", d.caps.max_cost_usd);
    println!("  max iters:    {}", d.caps.max_iterations);
    println!("  max duration: {} min", d.caps.max_duration_min);
    println!("runaway:");
    println!("  repeated-action: {}", d.runaway.repeated_action);
    println!("  error-streak:    {}", d.runaway.error_streak);
    println!("no-progress:");
    println!("  iterations:   {}", d.no_progress.iterations);
    println!(
        "  test command: {}",
        d.no_progress.test_command.as_deref().unwrap_or("(none)")
    );
}
