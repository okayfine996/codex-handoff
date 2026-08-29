use chrono::{DateTime, Utc};
use codex_handoff::{
    LocalHealth, ProfileListEntry, ProfileMetadata, ResetCredits, UsageBucket, UsageReport,
    UsageStatus, UsageWindow,
};
use comfy_table::{Cell, Color, ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};
use console::Style;
use std::{io::IsTerminal, path::Path};

const PROGRESS_WIDTH: usize = 10;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

pub fn default_color_mode() -> ColorMode {
    ColorMode::Auto
}

pub fn terminal_width() -> u16 {
    console::Term::stdout().size().1.clamp(64, 110)
}

pub fn render_list(entries: &[ProfileListEntry]) -> String {
    let width = terminal_width();
    let colors = colors_enabled(default_color_mode());
    entries
        .iter()
        .map(|entry| render_profile_with_colors(entry, width, colors))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn render_status(
    profile: &ProfileMetadata,
    usage: &UsageStatus,
    live_auth: &Path,
    vault: &Path,
) -> String {
    let width = terminal_width();
    let colors = colors_enabled(default_color_mode());
    let mut output = render_account(
        &AccountView {
            name: profile.name.as_str(),
            email: &profile.email,
            last_synced_at: Some(profile.last_synced_at),
            active: true,
            health: "ok",
        },
        usage,
        width,
        colors,
    );
    output.push_str("\n\n");
    output.push_str(&paint("Paths", Accent::Muted, colors));
    output.push_str(&format!(
        "\n  Live auth  {}\n  Vault      {}",
        live_auth.display(),
        vault.display()
    ));
    output
}

#[cfg(test)]
fn render_profile(entry: &ProfileListEntry, width: u16, mode: ColorMode) -> String {
    render_profile_with_colors(entry, width, colors_enabled(mode))
}

fn render_profile_with_colors(entry: &ProfileListEntry, width: u16, colors: bool) -> String {
    let (email, last_synced_at) = entry
        .metadata
        .as_ref()
        .map(|profile| (profile.email.as_str(), Some(profile.last_synced_at)))
        .unwrap_or(("<unavailable>", None));
    let health = health_label(&entry.health);
    render_account(
        &AccountView {
            name: entry.name.as_str(),
            email,
            last_synced_at,
            active: entry.active,
            health: &health,
        },
        &entry.usage,
        width,
        colors,
    )
}

fn render_account(
    account: &AccountView<'_>,
    usage: &UsageStatus,
    width: u16,
    colors: bool,
) -> String {
    let active_marker = if account.active {
        format!(" {}", paint("ACTIVE", Accent::Success, colors))
    } else {
        String::new()
    };
    let mut output = format!(
        "{}{}\n{}  {}\n{}  {}\n{}  {}",
        paint(account.name, Accent::Title, colors),
        active_marker,
        paint("Email", Accent::Muted, colors),
        account.email,
        paint("Last synced", Accent::Muted, colors),
        account
            .last_synced_at
            .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "<unavailable>".into()),
        paint("Health", Accent::Muted, colors),
        account.health
    );
    match usage {
        UsageStatus::Available(report) => {
            output.push_str("\n\n");
            output.push_str(&usage_table(report, width, colors));
            output.push_str(&usage_notes(report, colors));
        }
        UsageStatus::Unavailable(reason) => {
            output.push_str("\n\n");
            output.push_str(&paint("Usage unavailable", Accent::Warning, colors));
            output.push_str(&format!("\n  {reason}"));
        }
        UsageStatus::NotQueried => {
            output.push_str("\n\n");
            output.push_str(&paint("Usage not queried", Accent::Muted, colors));
        }
    }
    output
}

struct AccountView<'a> {
    name: &'a str,
    email: &'a str,
    last_synced_at: Option<DateTime<Utc>>,
    active: bool,
    health: &'a str,
}

fn usage_table(report: &UsageReport, width: u16, colors: bool) -> String {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_width(width);
    table.set_header(vec!["Window", "Usage", "Remaining", "Resets", "Duration"]);
    for bucket in &report.buckets {
        add_window_row(
            &mut table,
            bucket,
            "primary",
            bucket.primary.as_ref(),
            colors,
        );
        add_window_row(
            &mut table,
            bucket,
            "secondary",
            bucket.secondary.as_ref(),
            colors,
        );
    }
    table.to_string()
}

fn add_window_row(
    table: &mut Table,
    bucket: &UsageBucket,
    slot: &str,
    window: Option<&UsageWindow>,
    colors: bool,
) {
    let label = window_label(bucket, slot);
    let Some(window) = window else {
        table.add_row(vec![
            Cell::new(label),
            Cell::new("—"),
            Cell::new("—"),
            Cell::new("—"),
            Cell::new("—"),
        ]);
        return;
    };
    let accent = percentage_accent(window.used_percent);
    let usage = format!(
        "{} {}%",
        progress_bar(window.used_percent),
        window.used_percent
    );
    let remaining = format!("{}%", 100 - window.used_percent);
    let reset = window
        .resets_at
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|timestamp| timestamp.format("%b %-d %H:%M UTC").to_string())
        .unwrap_or_else(|| "—".into());
    let duration = window
        .window_duration_mins
        .map(format_duration)
        .unwrap_or_else(|| "—".into());
    table.add_row(vec![
        Cell::new(label),
        colored_cell(usage, accent, colors),
        colored_cell(remaining, accent, colors),
        Cell::new(reset),
        Cell::new(duration),
    ]);
}

fn usage_notes(report: &UsageReport, colors: bool) -> String {
    let mut notes = Vec::new();
    for bucket in &report.buckets {
        if let Some(reached) = &bucket.reached_type {
            notes.push(format!(
                "{}: {}",
                paint("Limit reached", Accent::Danger, colors),
                humanize(reached)
            ));
        }
        if bucket.spend_control_reached == Some(true) {
            notes.push(paint("Spend control reached", Accent::Danger, colors));
        }
    }
    if let Some(credits) = &report.reset_credits {
        notes.extend(render_reset_credits(credits, colors));
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", notes.join("\n"))
    }
}

fn render_reset_credits(credits: &ResetCredits, colors: bool) -> Vec<String> {
    if credits.available_count == 0 {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "{}: {}",
        paint("Reset credits", Accent::Success, colors),
        credits.available_count
    )];
    for credit in &credits.credits {
        let title = credit.title.as_deref().unwrap_or("Reset credit");
        let description = credit
            .description
            .as_deref()
            .map(|description| format!(" — {description}"))
            .unwrap_or_default();
        lines.push(format!("  {title} ({}){description}", credit.status));
    }
    lines
}

fn window_label(bucket: &UsageBucket, slot: &str) -> String {
    let base = match slot {
        "primary" => "5 hours",
        "secondary" => "Week",
        _ => slot,
    };
    if bucket.id == "codex" {
        base.into()
    } else {
        format!("{} / {base}", humanize(&bucket.id))
    }
}

fn progress_bar(used_percent: u8) -> String {
    let filled = ((used_percent as usize * PROGRESS_WIDTH) + 50) / 100;
    format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(PROGRESS_WIDTH - filled)
    )
}

fn format_duration(minutes: i64) -> String {
    match minutes {
        60.. if minutes % 60 == 0 => format!("{}h", minutes / 60),
        _ => format!("{minutes}m"),
    }
}

fn humanize(value: &str) -> String {
    value.replace('_', " ")
}

fn health_label(health: &LocalHealth) -> String {
    match health {
        LocalHealth::Healthy => "ok".into(),
        LocalHealth::Unhealthy(reason) => format!("error: {reason}"),
    }
}

fn colors_enabled(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
        }
    }
}

fn colored_cell(value: String, accent: Accent, colors: bool) -> Cell {
    let cell = Cell::new(value);
    if colors {
        match accent {
            Accent::Success => cell.fg(Color::Green),
            Accent::Warning => cell.fg(Color::Yellow),
            Accent::Danger => cell.fg(Color::Red),
            Accent::Title | Accent::Muted => cell,
        }
    } else {
        cell
    }
}

fn paint(value: &str, accent: Accent, colors: bool) -> String {
    let style = match accent {
        Accent::Title => Style::new().bold(),
        Accent::Muted => Style::new().dim(),
        Accent::Success => Style::new().green().bold(),
        Accent::Warning => Style::new().yellow().bold(),
        Accent::Danger => Style::new().red().bold(),
    };
    style.apply_to(value).force_styling(colors).to_string()
}

fn percentage_accent(used_percent: u8) -> Accent {
    match used_percent {
        0..=49 => Accent::Success,
        50..=79 => Accent::Warning,
        _ => Accent::Danger,
    }
}

#[derive(Clone, Copy)]
enum Accent {
    Title,
    Muted,
    Success,
    Warning,
    Danger,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use codex_handoff::{
        LocalHealth, ProfileListEntry, ProfileMetadata, ProfileName, ResetCredit, ResetCredits,
        UsageBucket, UsageReport, UsageStatus, UsageWindow,
    };

    #[test]
    fn renders_an_active_profile_as_a_compact_dashboard_without_ansi() {
        let profile = ProfileListEntry {
            name: ProfileName::parse("personal").unwrap(),
            metadata: Some(ProfileMetadata {
                schema_version: 1,
                name: ProfileName::parse("personal").unwrap(),
                email: "personal@example.com".into(),
                created_at: Utc.with_ymd_and_hms(2026, 8, 29, 10, 0, 0).unwrap(),
                last_synced_at: Utc.with_ymd_and_hms(2026, 8, 29, 11, 35, 0).unwrap(),
            }),
            active: true,
            health: LocalHealth::Healthy,
            usage: UsageStatus::Available(UsageReport {
                buckets: vec![UsageBucket {
                    id: "codex".into(),
                    primary: Some(UsageWindow {
                        used_percent: 18,
                        resets_at: Some(1_788_021_409),
                        window_duration_mins: Some(300),
                    }),
                    secondary: Some(UsageWindow {
                        used_percent: 82,
                        resets_at: Some(1_788_615_907),
                        window_duration_mins: Some(10_080),
                    }),
                    reached_type: None,
                    spend_control_reached: Some(false),
                }],
                reset_credits: Some(ResetCredits {
                    available_count: 1,
                    credits: vec![ResetCredit {
                        title: Some("Full reset".into()),
                        description: Some("Weekly + 5 hr".into()),
                        status: "available".into(),
                    }],
                }),
            }),
        };

        let output = render_profile(&profile, 88, ColorMode::Never);

        assert!(output.contains("ACTIVE"));
        assert!(output.contains("personal@example.com"));
        assert!(output.contains("5 hours"));
        assert!(output.contains("Week"));
        assert!(output.contains("Full reset"));
        assert!(!output.contains("spend control reached: false"));
        assert!(!output.contains('\u{1b}'));

        let colored_output = render_profile(&profile, 88, ColorMode::Always);
        assert!(colored_output.contains('\u{1b}'));
    }

    #[test]
    fn renders_usage_errors_as_a_warning_without_hiding_profile_metadata() {
        let profile = ProfileListEntry {
            name: ProfileName::parse("work").unwrap(),
            metadata: Some(ProfileMetadata {
                schema_version: 1,
                name: ProfileName::parse("work").unwrap(),
                email: "work@example.com".into(),
                created_at: Utc.with_ymd_and_hms(2026, 8, 29, 10, 0, 0).unwrap(),
                last_synced_at: Utc.with_ymd_and_hms(2026, 8, 29, 11, 35, 0).unwrap(),
            }),
            active: false,
            health: LocalHealth::Healthy,
            usage: UsageStatus::Unavailable("Codex rejected the usage request".into()),
        };

        let output = render_profile(&profile, 88, ColorMode::Never);

        assert!(output.contains("work@example.com"));
        assert!(output.contains("Usage unavailable"));
        assert!(output.contains("Codex rejected the usage request"));
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn renders_status_with_the_live_and_vault_paths() {
        let profile = ProfileMetadata {
            schema_version: 1,
            name: ProfileName::parse("personal").unwrap(),
            email: "personal@example.com".into(),
            created_at: Utc.with_ymd_and_hms(2026, 8, 29, 10, 0, 0).unwrap(),
            last_synced_at: Utc.with_ymd_and_hms(2026, 8, 29, 11, 35, 0).unwrap(),
        };

        let output = render_status(
            &profile,
            &UsageStatus::NotQueried,
            std::path::Path::new("/tmp/codex/auth.json"),
            std::path::Path::new("/tmp/codex-handoff"),
        );

        assert!(output.contains("personal@example.com"));
        assert!(output.contains("Usage not queried"));
        assert!(output.contains("Live auth  /tmp/codex/auth.json"));
        assert!(output.contains("Vault      /tmp/codex-handoff"));
    }
}
