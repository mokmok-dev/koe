//! `koe list` — enumerate capture-able apps.

use std::fmt::Write as _;

use clap::Parser;
use koe_core::{AppInfo, enumerate_apps, native_provider_registered};
use serde_json::{Value, json};

use super::Run;
use crate::MainError;

/// List capture-able apps and their audio activity.
#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Only show apps with active audio.
    #[arg(long)]
    audio_only: bool,

    /// Output as a JSON array.
    #[arg(long)]
    json: bool,
}

impl Run for ListArgs {
    fn run(self) -> Result<(), MainError> {
        if !native_provider_registered() {
            return Err(MainError::NativeBridgeUnavailable("list"));
        }

        let enumerated = enumerate_apps();
        if enumerated.is_empty() {
            eprintln!("note: no capture-able apps reported by the native provider");
        }
        let apps = filter_apps(enumerated, self.audio_only);
        if self.json {
            println!("{}", format_apps_json(&apps)?);
        } else {
            print!("{}", format_apps_table(&apps));
        }
        Ok(())
    }
}

fn filter_apps(
    apps: Vec<AppInfo>,
    audio_only: bool,
) -> Vec<AppInfo> {
    if audio_only {
        apps.into_iter().filter(|app| app.has_audio).collect()
    } else {
        apps
    }
}

fn format_apps_table(apps: &[AppInfo]) -> String {
    let mut out = String::from(
        "  PID    NAME                  BUNDLE ID               HAS AUDIO\n  ─────  ────────────────────  ──────────────────────  ─────────\n",
    );
    for app in apps {
        let name = sanitize_for_table(&app.name);
        let bundle = app
            .bundle_id
            .as_deref()
            .map_or_else(|| "-".to_owned(), sanitize_for_table);
        let has_audio = if app.has_audio { "yes" } else { "no" };
        let _ = writeln!(
            out,
            "  {:<5}  {:<20}  {:<22}  {}",
            app.pid,
            truncate(&name, 20),
            truncate(&bundle, 22),
            has_audio
        );
    }
    out
}

fn format_apps_json(apps: &[AppInfo]) -> Result<String, MainError> {
    let rows: Vec<Value> = apps
        .iter()
        .map(|app| {
            json!({
                "pid": app.pid,
                "name": app.name,
                "bundle_id": app.bundle_id,
                "has_audio": app.has_audio,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&rows)?)
}

/// Strip C0/C1 control characters so table layout cannot be broken.
fn sanitize_for_table(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

fn truncate(
    value: &str,
    max: usize,
) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_apps() -> Vec<AppInfo> {
        vec![
            AppInfo {
                pid: 4201,
                name: "Google Chrome".into(),
                bundle_id: Some("com.google.Chrome".into()),
                has_audio: true,
            },
            AppInfo {
                pid: 1234,
                name: "Finder".into(),
                bundle_id: Some("com.apple.Finder".into()),
                has_audio: false,
            },
            AppInfo {
                pid: 99,
                name: "Helper".into(),
                bundle_id: None,
                has_audio: false,
            },
        ]
    }

    #[test]
    fn audio_only_filters_silent_apps() {
        let filtered = filter_apps(sample_apps(), true);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pid, 4201);
    }

    #[test]
    fn audio_only_false_keeps_all() {
        assert_eq!(filter_apps(sample_apps(), false).len(), 3);
    }

    #[test]
    fn audio_only_empty_input_stays_empty() {
        assert!(filter_apps(Vec::new(), true).is_empty());
    }

    #[test]
    fn table_includes_header_and_rows() {
        let table = format_apps_table(&sample_apps());
        assert!(table.contains("PID"));
        assert!(table.contains("Google Chrome"));
        assert!(table.contains("yes"));
        assert!(table.contains("no"));
        assert!(table.contains("Helper"));
        assert!(table.contains("  99     Helper"));
        assert!(table.contains('-'));
    }

    #[test]
    fn table_strips_control_chars_and_truncates() {
        let apps = vec![AppInfo {
            pid: 1,
            name: "Bad\nName\x07WithControls".into(),
            bundle_id: Some("com.example.very.long.bundle.id.that.overflows".into()),
            has_audio: false,
        }];
        let table = format_apps_table(&apps);
        // header + rule + one data line
        assert_eq!(table.lines().count(), 3);
        assert!(!table.contains('\x07'));
        assert!(table.contains("BadNameWithControls") || table.contains("BadNameWithControl…"));
        assert!(table.contains('…'));
    }

    #[test]
    fn truncate_long_names() {
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn json_is_array_of_objects() {
        let json = format_apps_json(&sample_apps()).expect("json");
        let value: Value = serde_json::from_str(&json).expect("parse");
        let arr = value.as_array().expect("array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["pid"], 4201);
        assert_eq!(arr[0]["has_audio"], true);
        assert_eq!(arr[1]["bundle_id"], "com.apple.Finder");
        assert!(arr[2]["bundle_id"].is_null());
    }

    #[test]
    fn run_errors_without_native_provider() {
        let err = ListArgs {
            audio_only: false,
            json: false,
        }
        .run()
        .expect_err("must fail without provider");
        assert!(matches!(err, MainError::NativeBridgeUnavailable("list")));
    }
}
