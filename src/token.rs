use anyhow::{Result, bail};
use serde_json::Value;

use crate::api::RustGridClient;

pub struct GitHubTokenManager<'a> {
    api: &'a RustGridClient,
    run_id: &'a str,
    expected_repository: &'a str,
    required_permissions: &'a Value,
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
        }
    }

    /// Issues a fresh token for each privileged GitHub operation. RustGrid and
    /// GitHub remain authoritative for expiry; the secret is never persisted.
    pub fn token(&self) -> Result<String> {
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
        Ok(issued.token)
    }
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
}
