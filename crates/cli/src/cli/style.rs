// SPDX-License-Identifier: Apache-2.0
//! Tasteful terminal styling for Heddle CLI output.
//!
//! Heddle's brand voice ("precise, calm, conversational") translates
//! to a deliberately restrained terminal palette: dim/bright contrast
//! and bold weight do most of the structural work; saturated color
//! appears only at semantic seams (success/warning/error,
//! confidence band, identity vs. id). No rainbow output, no syntax
//! highlighting density.
//!
//! Color decisions are made **once** at CLI startup via
//! [`init_from_cli`], which consults — in precedence order:
//!
//! 1. `--no-color` CLI flag (force off)
//! 2. `NO_COLOR` env var (per <https://no-color.org>) — force off
//! 3. `CLICOLOR_FORCE=1` env var — force on, even on a non-TTY
//! 4. stdout isatty — auto-detected default
//!
//! The decision is stored in a process-wide [`OnceLock`] so render
//! sites consult a `bool` rather than re-querying the environment per
//! line. JSON output is *always* uncolored; that decision happens at
//! the print site, not here — `should_output_json` short-circuits
//! before any styled helper runs.

use std::{
    io::IsTerminal,
    sync::atomic::{AtomicI8, Ordering},
};

use anstyle::{Color, Style};

use super::cli_args::Cli;

/// Process-wide gate, encoded as a tristate atomic so tests can
/// override the value freely without rebuilding the cell.
///
/// - `0`  — uninitialized (treat as "color off" so we never leak
///   escapes into log files when `init_from_cli` was skipped)
/// - `1`  — color enabled
/// - `-1` — color disabled (explicit)
///
/// Atomic-relaxed is sufficient: the value is set once at startup
/// before any rendering begins, and tests use a single thread.
static COLOR_STATE: AtomicI8 = AtomicI8::new(0);

const STATE_OFF: i8 = -1;
const STATE_ON: i8 = 1;

/// Resolve the color decision once at CLI startup.
///
/// Subsequent calls overwrite the previous decision — tests need
/// this so they can flip the gate mid-process. Production only
/// calls this once, from `main`.
pub fn init_from_cli(cli: &Cli) {
    let enabled = decide_color_enabled(cli, &EnvProbe::real());
    COLOR_STATE.store(
        if enabled { STATE_ON } else { STATE_OFF },
        Ordering::Relaxed,
    );
}

/// Returns the active color decision. If `init_from_cli` was never
/// called (e.g. in a library test that bypasses `main`), this
/// defaults to `false` to avoid leaking escapes.
pub fn color_enabled() -> bool {
    COLOR_STATE.load(Ordering::Relaxed) == STATE_ON
}

/// Test-only override. Use this from any test that wants to assert
/// styled or unstyled output without depending on the ambient TTY
/// state.
#[cfg(test)]
pub(crate) fn force_for_test(enabled: bool) {
    COLOR_STATE.store(
        if enabled { STATE_ON } else { STATE_OFF },
        Ordering::Relaxed,
    );
}

/// Tiny env-var indirection so the decision logic stays unit-testable
/// without touching the real environment. Each closure-style accessor
/// returns the env value if set; `EnvProbe::real()` is the only
/// production constructor, but tests can build a literal struct.
struct EnvProbe<'a> {
    no_color: Option<&'a str>,
    clicolor_force: Option<&'a str>,
    is_tty: bool,
}

impl EnvProbe<'_> {
    fn real() -> EnvProbe<'static> {
        // We leak these strings deliberately — they live for the
        // duration of one decision call and are never observed
        // afterwards. The alternative (`String`) would require
        // generic lifetimes that aren't worth the complexity here.
        let no_color = std::env::var("NO_COLOR").ok().map(|s| {
            let leaked: &'static str = Box::leak(s.into_boxed_str());
            leaked
        });
        let clicolor_force = std::env::var("CLICOLOR_FORCE").ok().map(|s| {
            let leaked: &'static str = Box::leak(s.into_boxed_str());
            leaked
        });
        EnvProbe {
            no_color,
            clicolor_force,
            is_tty: std::io::stdout().is_terminal(),
        }
    }
}

fn decide_color_enabled(cli: &Cli, env: &EnvProbe<'_>) -> bool {
    // 1. Explicit CLI flag wins. The user typed `--no-color`; honour
    //    it regardless of any env var.
    if cli.no_color {
        return false;
    }
    // 2. `NO_COLOR` is the cross-tool standard
    //    (<https://no-color.org>). Any non-empty value disables.
    if let Some(v) = env.no_color
        && !v.is_empty()
    {
        return false;
    }
    // 3. `CLICOLOR_FORCE=1` is the conventional escape hatch for
    //    pipes that want color preserved (e.g. piping to `less -R`).
    //    We require literal "1" to match the convention used by
    //    `ls`, `grep`, and bat.
    if let Some(v) = env.clicolor_force
        && v == "1"
    {
        return true;
    }
    // 4. Otherwise: color iff stdout is an interactive TTY.
    env.is_tty
}

// =====================================================================
// Palette
// =====================================================================
//
// Brand calls for warm/technical, never the saturated 16-color
// defaults. We use anstyle's 8-bit (256-color) palette to land on
// muted, deliberate hues:
//
// - `accent`: ANSI 8-bit 71 — a warm sage/green, used for success,
//   "current", and confidence ≥ 0.9. Cooler than 34 (lime) and warmer
//   than 28 (forest); reads well on both light and dark terminals.
// - `warn`:   ANSI 8-bit 178 — a warm amber, mid-warning. Avoids the
//   safety-vest 220 (yellow) and the orange 208 which reads as error.
// - `error`:  ANSI 8-bit 167 — a muted rust/terracotta. Cooler and
//   more deliberate than the default red 9; signals failure without
//   shouting.
// - `dim`:    standard "faint" weight — terminal-theme aware, since
//   8-bit grays clash with light backgrounds.
// - `bold`:   standard bold weight, no color shift.

const ACCENT_COLOR: Color = Color::Ansi256(anstyle::Ansi256Color(71));
const WARN_COLOR: Color = Color::Ansi256(anstyle::Ansi256Color(178));
const ERROR_COLOR: Color = Color::Ansi256(anstyle::Ansi256Color(167));

fn accent_style() -> Style {
    Style::new().fg_color(Some(ACCENT_COLOR))
}

fn warn_style() -> Style {
    Style::new().fg_color(Some(WARN_COLOR))
}

fn error_style() -> Style {
    Style::new().fg_color(Some(ERROR_COLOR))
}

fn dim_style() -> Style {
    Style::new().dimmed()
}

fn bold_style() -> Style {
    Style::new().bold()
}

// =====================================================================
// Helpers
// =====================================================================
//
// All helpers return `String`. We could return `impl Display` to
// avoid the allocation, but `Style` doesn't implement `Display` on its
// own — it expects a wrapped payload — and the call-site ergonomics
// (passing into `format!`/`println!`) are cleaner with a concrete
// `String`. Cost is one heap allocation per styled fragment, which
// is negligible against the syscall cost of writing to a terminal.

fn paint(style: Style, s: &str) -> String {
    if !color_enabled() {
        return s.to_string();
    }
    format!("{}{}{}", style.render(), s, style.render_reset())
}

/// Success/positive/current — warm sage/green (ANSI 8-bit 71).
pub fn accent(s: &str) -> String {
    paint(accent_style(), s)
}

/// Mid-warning — warm amber (ANSI 8-bit 178).
pub fn warn(s: &str) -> String {
    paint(warn_style(), s)
}

/// Hard error — muted rust (ANSI 8-bit 167).
pub fn error(s: &str) -> String {
    paint(error_style(), s)
}

/// De-emphasis — used for IDs, timestamps, paths, and other text
/// that's structurally important but shouldn't draw the eye.
pub fn dim(s: &str) -> String {
    paint(dim_style(), s)
}

/// Structural emphasis — intent text, headers, the principal name.
pub fn bold(s: &str) -> String {
    paint(bold_style(), s)
}

/// Section heading used for human output blocks.
pub fn section(s: &str) -> String {
    bold(s)
}

/// Small successful status marker. Keep the word short so it scans
/// like a status glyph but still works in plain terminals.
pub fn ok_marker() -> String {
    accent("[ok]")
}

/// Small in-progress status marker.
pub fn working_marker() -> String {
    warn("[working]")
}

/// Small warning status marker.
pub fn warn_marker() -> String {
    warn("[warn]")
}

/// Small failure status marker.
pub fn error_marker() -> String {
    error("[error]")
}

/// Render a calm label/value row.
pub fn field(label: &str, value: &str) -> String {
    format!("{} {}", dim(&format!("{label}:")), value)
}

/// Render a compact count with the number emphasized.
pub fn count(value: usize, noun: &str) -> String {
    let suffix = if value == 1 { "" } else { "s" };
    format!("{} {noun}{suffix}", bold(&value.to_string()))
}

/// Confidence band: maps the recorded numeric value to a semantic
/// color. Render the formatted text yourself (e.g. via
/// `format_confidence`) and pass it here; this keeps the formatting
/// rule in `repo` and the styling rule here.
pub fn confidence(value: Option<f32>, formatted: &str) -> String {
    match value {
        None => dim(formatted),
        Some(v) if v >= 0.9 => accent(formatted),
        Some(v) if v >= 0.75 => warn(formatted),
        Some(_) => error(formatted),
    }
}

/// Change-id styling: dim. We don't apply a monospace marker here —
/// terminals already render text monospaced. The "dim+monospace"
/// label in the spec was about *visual treatment*, which the
/// terminal grants for free.
pub fn change_id(id: &str) -> String {
    dim(id)
}

/// Principal styling: name in bold, email dimmed. Returns the
/// pre-composed `"Name <email>"` string so callers don't have to
/// thread two fragments through `println!` arguments.
pub fn principal(name: &str, email: &str) -> String {
    if !color_enabled() {
        return format!("{} <{}>", name, email);
    }
    format!("{} <{}>", bold(name), dim(email))
}

/// Thread-state styling: `active`/`ready`/`promoted` are accent;
/// `merged`/`abandoned` are dim (historical, not current);
/// `blocked`/`stale`/`draft` are warn. Unknown variants fall back
/// to plain text. The matcher is case-insensitive against the
/// `Display` form so callers can pass `state.to_string()` directly.
pub fn thread_state(state: &str) -> String {
    match state.to_ascii_lowercase().as_str() {
        "active" | "ready" | "promoted" | "current" => accent(state),
        "merged" | "abandoned" => dim(state),
        "blocked" | "stale" | "draft" | "diverged" => warn(state),
        _ => state.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    /// All helpers must return ANSI-free strings when color is off.
    /// Important: every render site relies on this — if the gate
    /// regresses, escape codes leak into log files, JSON pipelines,
    /// and test fixtures.
    ///
    /// Tests in this module touch a shared atomic (`COLOR_STATE`)
    /// so we serialize them under a single name to keep one test's
    /// `force_for_test` from racing another's read.
    #[test]
    #[serial(color_state)]
    fn helpers_emit_no_ansi_when_disabled() {
        force_for_test(false);
        for s in [
            accent("ok"),
            warn("careful"),
            error("boom"),
            dim("hd-abc123"),
            bold("Capture audit pipeline"),
            confidence(Some(0.95), "0.95"),
            confidence(None, "—"),
            change_id("hd-abc123"),
            principal("Ada Lovelace", "ada@analytical.engine"),
            thread_state("active"),
        ] {
            assert!(!s.contains('\x1b'), "expected no ANSI escape in {:?}", s);
        }
    }

    /// With color enabled, each helper emits an escape prefix.
    #[test]
    #[serial(color_state)]
    fn helpers_emit_ansi_when_enabled() {
        force_for_test(true);
        for s in [
            accent("ok"),
            warn("careful"),
            error("boom"),
            dim("hd-abc123"),
            bold("Capture audit pipeline"),
            confidence(Some(0.95), "0.95"),
            change_id("hd-abc123"),
            principal("Ada Lovelace", "ada@analytical.engine"),
            thread_state("active"),
        ] {
            assert!(s.contains('\x1b'), "expected ANSI escape in {:?}", s);
        }
    }

    /// Unknown thread-state strings render plain — we don't want
    /// to invent semantics for a state the matcher doesn't know.
    #[test]
    #[serial(color_state)]
    fn thread_state_unknown_is_plain() {
        force_for_test(true);
        let out = thread_state("zorblax");
        assert_eq!(out, "zorblax", "unknown state should not be styled");
    }

    /// Confidence bands map to the documented thresholds.
    #[test]
    #[serial(color_state)]
    fn confidence_bands() {
        force_for_test(true);
        // None → dim
        let none = confidence(None, "—");
        assert!(
            none.contains("\x1b[2m"),
            "None should be dimmed: {:?}",
            none
        );

        // ≥0.9 → accent (sage 71)
        let high = confidence(Some(0.95), "0.95");
        assert!(high.contains("38;5;71"), "high should be sage: {:?}", high);

        // ≥0.75 and <0.9 → warn (amber 178)
        let mid = confidence(Some(0.80), "0.80");
        assert!(mid.contains("38;5;178"), "mid should be amber: {:?}", mid);

        // <0.75 → error (rust 167)
        let low = confidence(Some(0.50), "0.50");
        assert!(low.contains("38;5;167"), "low should be rust: {:?}", low);
    }

    /// Decision logic: `--no-color` overrides every other signal,
    /// `NO_COLOR` overrides `CLICOLOR_FORCE`, and TTY auto-detect
    /// is the fallback.
    #[test]
    fn decision_no_color_flag_wins() {
        let cli = test_cli(true);
        let env = EnvProbe {
            no_color: None,
            clicolor_force: Some("1"),
            is_tty: true,
        };
        assert!(!decide_color_enabled(&cli, &env));
    }

    #[test]
    fn decision_no_color_env_overrides_force() {
        let cli = test_cli(false);
        let env = EnvProbe {
            no_color: Some("1"),
            clicolor_force: Some("1"),
            is_tty: true,
        };
        assert!(
            !decide_color_enabled(&cli, &env),
            "NO_COLOR must beat CLICOLOR_FORCE per no-color.org precedence"
        );
    }

    #[test]
    fn decision_force_color_overrides_non_tty() {
        let cli = test_cli(false);
        let env = EnvProbe {
            no_color: None,
            clicolor_force: Some("1"),
            is_tty: false,
        };
        assert!(decide_color_enabled(&cli, &env));
    }

    #[test]
    fn decision_non_tty_default_off() {
        let cli = test_cli(false);
        let env = EnvProbe {
            no_color: None,
            clicolor_force: None,
            is_tty: false,
        };
        assert!(!decide_color_enabled(&cli, &env));
    }

    #[test]
    fn decision_tty_default_on() {
        let cli = test_cli(false);
        let env = EnvProbe {
            no_color: None,
            clicolor_force: None,
            is_tty: true,
        };
        assert!(decide_color_enabled(&cli, &env));
    }

    /// Empty `NO_COLOR` is the documented opt-out — per
    /// no-color.org, "the value of `NO_COLOR` is irrelevant if it's
    /// non-empty"; an empty string is *not* a disable. We honour
    /// that subtlety so users can `NO_COLOR= cargo run` to reset
    /// without unsetting.
    #[test]
    fn decision_empty_no_color_is_not_disable() {
        let cli = test_cli(false);
        let env = EnvProbe {
            no_color: Some(""),
            clicolor_force: None,
            is_tty: true,
        };
        assert!(decide_color_enabled(&cli, &env));
    }

    fn test_cli(no_color: bool) -> Cli {
        // We can't easily construct `Cli` directly because it has a
        // mandatory subcommand; route through clap's parser with a
        // minimal valid argv. `--no-color` is a global flag so it
        // lands regardless of which subcommand we pick.
        use clap::Parser;
        let mut argv = vec!["heddle".to_string()];
        if no_color {
            argv.push("--no-color".to_string());
        }
        argv.push("status".to_string());
        Cli::try_parse_from(argv).expect("parse minimal cli")
    }

    /// Crucial: `principal()` with color off returns *exactly* the
    /// same string the un-styled call site would have produced.
    /// Render-site tests rely on this byte-for-byte equivalence.
    #[test]
    #[serial(color_state)]
    fn principal_uncolored_is_identity() {
        force_for_test(false);
        let out = principal("Ada Lovelace", "ada@analytical.engine");
        assert_eq!(out, "Ada Lovelace <ada@analytical.engine>");
    }

    #[test]
    #[serial(color_state)]
    fn change_id_uncolored_is_identity() {
        force_for_test(false);
        assert_eq!(change_id("hd-abc123"), "hd-abc123");
    }
}
