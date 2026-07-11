use std::{
    cell::RefCell,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::api::RustGridClient;

pub struct GitHubTokenManager<'a> {
    api: &'a RustGridClient,
    run_id: &'a str,
    expected_repository: &'a str,
    required_permissions: &'a Value,
    cached: RefCell<Option<CachedToken>>,
}

struct CachedToken {
    value: String,
    refresh_at: Instant,
}

impl<'a> GitHubTokenManager<'a> {
    pub fn new(
        api: &'a RustGridClient,
        run_id: &'a str,
        expected_repository: &'a str,
        required_permissions: &'a Value,
    ) -> Self {
        Self {
            api,
            run_id,
            expected_repository,
            required_permissions,
            cached: RefCell::new(None),
        }
    }

    pub fn token(&self) -> Result<String> {
        if let Some(cached) = self.cached.borrow().as_ref()
            && Instant::now() < cached.refresh_at
        {
            return Ok(cached.value.clone());
        }
        let issued = self.api.issue_github_token(self.run_id)?;
        if !issued
            .repository
            .eq_ignore_ascii_case(self.expected_repository)
        {
            bail!(
                "RustGrid GitHub token repository {} does not match manifest {}",
                issued.repository,
                self.expected_repository
            );
        }
        if !permissions_satisfy(self.required_permissions, &issued.permissions) {
            bail!("RustGrid GitHub token permissions do not satisfy the execution manifest");
        }
        let expires_at = parse_rfc3339_utc(&issued.expires_at)
            .context("RustGrid returned an invalid GitHub token expiry")?;
        let usable = expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            .saturating_sub(Duration::from_secs(120));
        let value = issued.token;
        self.cached.replace(Some(CachedToken {
            value: value.clone(),
            refresh_at: Instant::now() + usable,
        }));
        Ok(value)
    }
}

fn parse_rfc3339_utc(value: &str) -> Result<SystemTime> {
    let core = value
        .strip_suffix('Z')
        .context("GitHub token expiry must use UTC Z notation")?;
    let core = core.split('.').next().unwrap_or(core);
    let (date, time) = core.split_once('T').context("missing T separator")?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let year = date.next().context("missing year")??;
    let month = date.next().context("missing month")??;
    let day = date.next().context("missing day")??;
    let mut time = time.split(':').map(str::parse::<u64>);
    let hour = time.next().context("missing hour")??;
    let minute = time.next().context("missing minute")??;
    let second = time.next().context("missing second")??;
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => 0,
    };
    if day < 1 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        bail!("GitHub token expiry is out of range");
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    if days < 0 {
        bail!("GitHub token expiry predates Unix epoch");
    }
    let seconds = days as u64 * 86_400 + hour * 3600 + minute * 60 + second;
    Ok(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn permissions_satisfy(required: &Value, issued: &Value) -> bool {
    let Some(required) = required.as_object() else {
        return required.is_null();
    };
    let Some(issued) = issued.as_object() else {
        return required.is_empty();
    };
    required
        .iter()
        .all(|(name, value)| issued.get(name) == Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_brokered_permissions() {
        assert!(permissions_satisfy(
            &json!({"contents": "write"}),
            &json!({"contents": "write", "pull_requests": "write"})
        ));
        assert!(!permissions_satisfy(
            &json!({"contents": "write"}),
            &json!({"contents": "read"})
        ));
    }

    #[test]
    fn parses_github_expiry_timestamp() {
        let parsed = parse_rfc3339_utc("1970-01-01T00:01:00Z").unwrap();
        assert_eq!(parsed.duration_since(UNIX_EPOCH).unwrap().as_secs(), 60);
        assert!(parse_rfc3339_utc("not-a-date").is_err());
        assert!(parse_rfc3339_utc("2026-02-30T00:00:00Z").is_err());
    }
}
