use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub command: String,
    pub line: usize,
    minute: Field,
    hour: Field,
    day_of_month: Field,
    month: Field,
    day_of_week: Field,
}

#[derive(Clone, Debug)]
struct Field {
    values: Vec<u32>,
    wildcard: bool,
}

impl Field {
    fn contains(&self, value: u32) -> bool {
        self.values.binary_search(&value).is_ok()
    }
}

impl Job {
    pub fn matches(&self, time: DateTime<Utc>, zone: Tz) -> bool {
        let local = time.with_timezone(&zone);
        self.matches_local_date(local.date_naive())
            && self.minute.contains(local.minute())
            && self.hour.contains(local.hour())
    }

    pub fn next_after(&self, time: DateTime<Utc>, zone: Tz) -> Option<DateTime<Utc>> {
        let start = time.with_timezone(&zone).date_naive();
        // Every Gregorian calendar pattern repeats within 400 years. Iterating
        // dates and only the selected times avoids scanning every UTC minute.
        for offset in 0..146_098 {
            let Some(date) = start.checked_add_days(Days::new(offset)) else {
                break;
            };
            if !self.matches_local_date(date) {
                continue;
            }

            let mut earliest: Option<DateTime<Utc>> = None;
            for &hour in &self.hour.values {
                for &minute in &self.minute.values {
                    let Some(local) = date.and_hms_opt(hour, minute, 0) else {
                        continue;
                    };
                    let mut consider = |candidate: DateTime<Tz>| {
                        let candidate = candidate.with_timezone(&Utc);
                        if candidate > time && earliest.is_none_or(|old| candidate < old) {
                            earliest = Some(candidate);
                        }
                    };
                    match zone.from_local_datetime(&local) {
                        chrono::LocalResult::Single(value) => consider(value),
                        chrono::LocalResult::Ambiguous(first, second) => {
                            consider(first);
                            consider(second);
                        }
                        chrono::LocalResult::None => {}
                    }
                }
            }
            if earliest.is_some() {
                return earliest;
            }
        }
        None
    }

    fn matches_local_date(&self, date: NaiveDate) -> bool {
        let dom_matches = self.day_of_month.contains(date.day());
        let dow_matches = self
            .day_of_week
            .contains(date.weekday().num_days_from_sunday());
        let day_matches = if self.day_of_month.wildcard || self.day_of_week.wildcard {
            dom_matches && dow_matches
        } else {
            dom_matches || dow_matches
        };
        day_matches && self.month.contains(date.month())
    }
}

pub fn parse(input: &str) -> Result<Vec<Job>> {
    let mut seen = HashMap::new();
    let mut jobs = Vec::new();
    for (index, raw) in input.lines().enumerate() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let job = parse_line(line, number).with_context(|| format!("line {number}"))?;
        if let Some(previous) = seen.insert(job.id.clone(), number) {
            bail!("line {number}: duplicate job from line {previous}");
        }
        jobs.push(job);
    }
    Ok(jobs)
}

fn parse_line(line: &str, line_number: usize) -> Result<Job> {
    if line.starts_with('@') {
        bail!(
            "unsupported cron directive `{}`",
            line.split_whitespace().next().unwrap_or(line)
        );
    }
    let mut tokens = line.split_whitespace();
    let first = tokens.next().context("expected five schedule fields")?;
    if first.contains('=') {
        bail!("environment assignments are unsupported; provide a five-field schedule and command");
    }
    let mut fields = vec![first];
    for _ in 1..5 {
        fields.push(tokens.next().context("expected five schedule fields")?);
    }
    let mut command_start = 0;
    for field in &fields {
        command_start += line[command_start..].find(*field).unwrap() + field.len();
    }
    let command = line[command_start..].trim_start();
    if command.is_empty() {
        bail!("missing command after five schedule fields");
    }
    let command = shell_command(command)?;

    let minute = parse_field(fields[0], 0, 59, None).context("minute field")?;
    let hour = parse_field(fields[1], 0, 23, None).context("hour field")?;
    let day_of_month = parse_field(fields[2], 1, 31, None).context("day-of-month field")?;
    let month = parse_field(fields[3], 1, 12, Some(&MONTHS)).context("month field")?;
    let mut day_of_week =
        parse_field(fields[4], 0, 7, Some(&WEEKDAYS)).context("day-of-week field")?;
    day_of_week.values = day_of_week
        .values
        .into_iter()
        .map(|day| if day == 7 { 0 } else { day })
        .collect();
    day_of_week.values.sort_unstable();
    day_of_week.values.dedup();
    let normalized = fields.join(" ");
    let digest = Sha256::digest(format!("{normalized}\0{command}").as_bytes());
    let id = format!("{digest:x}");

    Ok(Job {
        id,
        command,
        line: line_number,
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
    })
}

fn shell_command(input: &str) -> Result<String> {
    let mut command = String::with_capacity(input.len());
    let mut escaped = false;
    for ch in input.chars() {
        match ch {
            '\0' => bail!("NUL is not allowed in a command"),
            '%' if !escaped => {
                bail!("unescaped percent is unsupported (cron uses `%` to split stdin)")
            }
            '%' => {
                command.pop(); // Cron strips the escape before passing % to the shell.
                command.push(ch);
                escaped = false;
            }
            '\\' => {
                command.push(ch);
                escaped = !escaped;
            }
            _ => {
                command.push(ch);
                escaped = false;
            }
        }
    }
    Ok(command)
}

const MONTHS: [(&str, u32); 12] = [
    ("JAN", 1),
    ("FEB", 2),
    ("MAR", 3),
    ("APR", 4),
    ("MAY", 5),
    ("JUN", 6),
    ("JUL", 7),
    ("AUG", 8),
    ("SEP", 9),
    ("OCT", 10),
    ("NOV", 11),
    ("DEC", 12),
];
const WEEKDAYS: [(&str, u32); 7] = [
    ("SUN", 0),
    ("MON", 1),
    ("TUE", 2),
    ("WED", 3),
    ("THU", 4),
    ("FRI", 5),
    ("SAT", 6),
];

fn parse_field(input: &str, min: u32, max: u32, names: Option<&[(&str, u32)]>) -> Result<Field> {
    let wildcard = input.starts_with('*');
    let mut values = Vec::new();
    for item in input.split(',') {
        if item.is_empty() {
            bail!("empty list item in `{input}`");
        }
        let mut step_parts = item.split('/');
        let base = step_parts.next().unwrap();
        let step = match step_parts.next() {
            Some(value) => {
                let parsed = value
                    .parse::<u32>()
                    .context("step must be a positive integer")?;
                if parsed == 0 {
                    bail!("step must be greater than zero");
                }
                parsed
            }
            None => 1,
        };
        if step_parts.next().is_some() {
            bail!("too many `/` separators in `{item}`");
        }
        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((left, right)) = base.split_once('-') {
            let start = parse_value(left, names).context("invalid range start")?;
            let end = parse_value(right, names).context("invalid range end")?;
            (start, end)
        } else {
            let value = parse_value(base, names).context("invalid value")?;
            if item.contains('/') {
                (value, max)
            } else {
                (value, value)
            }
        };
        if start < min || start > max || end < min || end > max {
            bail!("value outside allowed range {min}..={max} in `{item}`");
        }
        if start > end {
            bail!("range start exceeds end in `{item}`");
        }
        let mut value = start;
        while value <= end {
            values.push(value);
            match value.checked_add(step) {
                Some(next) => value = next,
                None => break,
            }
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(Field { values, wildcard })
}

fn parse_value(input: &str, names: Option<&[(&str, u32)]>) -> Result<u32> {
    if let Some((_, value)) = names.and_then(|names| {
        names
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(input))
    }) {
        return Ok(*value);
    }
    input
        .parse()
        .with_context(|| format!("invalid value `{input}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(spec: &str) -> Job {
        parse(spec).unwrap().remove(0)
    }

    #[test]
    fn parses_names_ranges_lists_steps_and_stable_ids() {
        let a = parse("  */15 9-17/2 1,15 jan,Mar mon-fri  echo hi\n").unwrap();
        let b = parse("*/15 9-17/2 1,15 jan,Mar mon-fri echo hi").unwrap();
        assert_eq!(a[0].id, b[0].id);
        assert_eq!(a[0].command, "echo hi");
        assert_eq!(a[0].line, 1);
        assert_eq!(a[0].minute.values, vec![0, 15, 30, 45]);
    }

    #[test]
    fn day_of_month_and_weekday_use_or_semantics() {
        let job = job("0 0 1 * MON run");
        assert!(job.matches("2024-04-01T00:00:00Z".parse().unwrap(), chrono_tz::UTC));
        assert!(!job.matches("2024-04-02T00:00:00Z".parse().unwrap(), chrono_tz::UTC));
        assert!(job.matches("2024-04-08T00:00:00Z".parse().unwrap(), chrono_tz::UTC));
    }

    #[test]
    fn stepped_wildcard_keeps_its_selected_days() {
        let job = job("0 0 */2 * * run");
        assert!(job.matches("2024-04-01T00:00:00Z".parse().unwrap(), chrono_tz::UTC));
        assert!(!job.matches("2024-04-02T00:00:00Z".parse().unwrap(), chrono_tz::UTC));
    }

    #[test]
    fn supports_escaped_percent_and_rejects_duplicate_jobs() {
        let command = job(r"* * * * * printf '\%s' foo").command;
        assert_eq!(command, "printf '%s' foo");
        assert!(parse(r"* * * * * echo \\%").is_err());
        let duplicate = parse("* * * * * run\n# a comment\n* * * * * run").unwrap_err();
        assert!(
            duplicate
                .to_string()
                .contains("line 3: duplicate job from line 1")
        );
    }

    #[test]
    fn handles_dst_gap_and_repeated_hour() {
        let zone = chrono_tz::America::New_York;
        let daily_gap = job("30 2 * * * run");
        let after_gap: DateTime<Utc> = "2024-03-10T06:59:00Z".parse().unwrap();
        assert_eq!(
            daily_gap.next_after(after_gap, zone).unwrap().to_rfc3339(),
            "2024-03-11T06:30:00+00:00"
        );

        let repeated = job("30 1 * * * run");
        let between_folds: DateTime<Utc> = "2024-11-03T05:45:00Z".parse().unwrap();
        assert_eq!(
            repeated
                .next_after(between_folds, zone)
                .unwrap()
                .to_rfc3339(),
            "2024-11-03T06:30:00+00:00"
        );
    }

    #[test]
    fn impossible_calendar_schedule_has_no_occurrence() {
        let impossible = job("0 0 30 FEB * run");
        let start: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        assert_eq!(impossible.next_after(start, chrono_tz::UTC), None);
    }

    #[test]
    fn reports_unsupported_inputs_and_bad_fields() {
        assert!(format!("{:#}", parse("@daily run").unwrap_err()).contains("line 1"));
        assert!(format!("{:#}", parse("FOO=bar\n").unwrap_err()).contains("environment"));
        assert!(format!("{:#}", parse("* * * * * echo %data").unwrap_err()).contains("percent"));
        assert!(format!("{:#}", parse("61 * * * * run").unwrap_err()).contains("minute"));
    }
}
