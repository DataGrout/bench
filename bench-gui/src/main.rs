//! Bench — a DSP workbench that runs on DataGrout.

mod app;
mod icon;
mod scope;

use std::path::{Path, PathBuf};

use app::BenchApp;

fn main() -> eframe::Result<()> {
    let initial_profile = initial_profile_from_args();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([880.0, 560.0])
            .with_title("Bench")
            .with_icon(icon::icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Bench",
        options,
        Box::new(move |cc| Ok(Box::new(BenchApp::new(cc, initial_profile)))),
    )
}

const USAGE: &str = "usage: dgbench [--profile PATH | PATH.bench]

  --profile, -p PATH   launch with the bench set up as the profile describes
  PATH.bench           the same, by extension alone
  --help, -h           this text

Profiles are JSON files; the shipped examples are in bench-core/examples/profiles/ and in the
Profiles menu. Saved ones go to ~/Documents/Bench/profiles.";

/// `dgbench [--profile PATH | PATH.bench]`.
///
/// Hand-rolled rather than a CLI crate: there is one option, and a reader of
/// an example app should not have to learn an argument parser to find it.
fn initial_profile_from_args() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    let mut profile = None;
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        match text.as_ref() {
            "--profile" | "-p" => match args.next() {
                Some(path) => profile = Some(PathBuf::from(path)),
                None => usage_and_exit("--profile needs a path"),
            },
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ if bench_core::profile::is_profile_path(Path::new(&arg)) => {
                profile = Some(PathBuf::from(&arg));
            }
            other => usage_and_exit(&format!("unrecognised argument: {other}")),
        }
    }
    profile
}

fn usage_and_exit(problem: &str) -> ! {
    eprintln!("dgbench: {problem}\n\n{USAGE}");
    std::process::exit(2);
}
