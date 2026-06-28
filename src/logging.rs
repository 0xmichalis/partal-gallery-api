use std::fmt;

use time::{macros::format_description, OffsetDateTime, UtcOffset};
use tracing::Level;
use tracing_subscriber::fmt::format::{FormatFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Info => Level::INFO,
            LogLevel::Warn => Level::WARN,
            LogLevel::Error => Level::ERROR,
        }
    }
}

/// Determine whether colored output should be enabled.
///
/// Precedence follows the NO_COLOR (https://no-color.org/) and FORCE_COLOR
/// (https://force-color.org/) conventions:
/// 1. `NO_COLOR` set (any value) -> disable
/// 2. `FORCE_COLOR` set (any value) -> enable
/// 3. otherwise use the CLI flag (inverted, since the flag is `--no-color`)
pub fn should_enable_color(cli_no_color: bool) -> bool {
    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    if std::env::var("FORCE_COLOR").is_ok() {
        return true;
    }
    !cli_no_color
}

static TIME_FORMAT: &[time::format_description::FormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:2]Z");

pub fn init(log_level: LogLevel, enable_color: bool) {
    tracing_subscriber::fmt()
        .with_level(true)
        .with_ansi(enable_color)
        .with_max_level(Level::from(log_level))
        .with_file(false)
        .with_line_number(false)
        .with_target(false)
        .event_format(CustomFormat)
        .init();
}

struct CustomFormat;

impl<S, N> FormatEvent<S, N> for CustomFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        // Print timestamp
        let now = OffsetDateTime::now_utc().to_offset(UtcOffset::UTC);
        if writer.has_ansi_escapes() {
            write!(
                writer,
                "\x1b[96m{}\x1b[0m ",
                now.format(TIME_FORMAT).unwrap_or("".into())
            )?;
        } else {
            write!(writer, "{} ", now.format(TIME_FORMAT).unwrap_or("".into()))?;
        }

        // Print log level
        let level = *event.metadata().level();
        if writer.has_ansi_escapes() {
            let color = match level {
                Level::ERROR => "\x1b[31m", // Red
                Level::WARN => "\x1b[33m",  // Yellow
                Level::INFO => "\x1b[32m",  // Green
                Level::DEBUG => "\x1b[36m", // Cyan
                _ => "\x1b[0m",             // Default
            };
            write!(writer, "{color}{level:>5}\x1b[0m ")?;
        } else {
            write!(writer, "{level:>5} ")?;
        }

        // Print the log message
        ctx.format_fields(writer.by_ref(), event)?;

        // Print span fields at the end
        if let Some(scope) = ctx.event_scope() {
            let ansi_regex = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
            for span in scope.from_root() {
                let exts = span.extensions();
                let Some(fields) = exts.get::<FormattedFields<N>>() else {
                    continue;
                };
                let fields = fields.fields.as_str();
                if fields.is_empty() {
                    continue;
                }
                if !writer.has_ansi_escapes() {
                    write!(writer, " {fields}")?;
                } else {
                    let colored: String = fields
                        .split_whitespace()
                        .map(|pair| {
                            let mut parts = pair.splitn(2, '=');
                            if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                                let clean_key = ansi_regex.replace_all(key, "");
                                let clean_value = ansi_regex.replace_all(value, "");
                                format!("\x1b[36m{clean_key}\x1b[0m=\x1b[95m{clean_value}\x1b[0m",)
                            } else {
                                pair.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    write!(writer, " {colored}")?;
                }
            }
        }
        writeln!(writer)
    }
}

#[cfg(test)]
mod from_loglevel_tests {
    #[test]
    fn maps_loglevel_to_tracing_level() {
        assert_eq!(
            tracing::Level::from(super::LogLevel::Debug),
            tracing::Level::DEBUG
        );
        assert_eq!(
            tracing::Level::from(super::LogLevel::Info),
            tracing::Level::INFO
        );
        assert_eq!(
            tracing::Level::from(super::LogLevel::Warn),
            tracing::Level::WARN
        );
        assert_eq!(
            tracing::Level::from(super::LogLevel::Error),
            tracing::Level::ERROR
        );
    }
}
