use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenConsumption {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenConsumption {
    pub fn total_tokens(self) -> Result<u64> {
        self.input_tokens
            .checked_add(self.output_tokens)
            .context("token consumption total overflowed")
    }

    pub fn observe_codex_jsonl(&mut self, line: &str) -> Result<bool> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return Ok(false);
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("turn.completed") {
            return Ok(false);
        }
        let Some(usage) = value.get("usage") else {
            return Ok(false);
        };
        let input_tokens = read_count(usage, "input_tokens")?;
        let cached_input_tokens = read_count(usage, "cached_input_tokens")?;
        let output_tokens = read_count(usage, "output_tokens")?;
        if cached_input_tokens > input_tokens {
            anyhow::bail!("Codex reported cached_input_tokens greater than input_tokens");
        }
        self.add_counts(input_tokens, cached_input_tokens, output_tokens)?;
        Ok(true)
    }

    pub fn add_counts(
        &mut self,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        if cached_input_tokens > input_tokens {
            anyhow::bail!("cached_input_tokens cannot exceed input_tokens");
        }
        self.input_tokens = self
            .input_tokens
            .checked_add(input_tokens)
            .context("input token consumption overflowed")?;
        self.cached_input_tokens = self
            .cached_input_tokens
            .checked_add(cached_input_tokens)
            .context("cached input token consumption overflowed")?;
        self.output_tokens = self
            .output_tokens
            .checked_add(output_tokens)
            .context("output token consumption overflowed")?;
        self.total_tokens()?;
        Ok(())
    }
}

fn read_count(value: &serde_json::Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .with_context(|| format!("Codex turn.completed usage omitted valid {field}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_completed_codex_turns_only() {
        let mut consumption = TokenConsumption::default();
        assert!(consumption
            .observe_codex_jsonl(
                r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":25}}"#,
            )
            .unwrap());
        assert!(
            !consumption
                .observe_codex_jsonl(r#"{"type":"item.completed","item":{"type":"agent_message"}}"#)
                .unwrap()
        );
        assert!(consumption
            .observe_codex_jsonl(
                r#"{"type":"turn.completed","usage":{"input_tokens":30,"cached_input_tokens":10,"output_tokens":5}}"#,
            )
            .unwrap());
        assert_eq!(
            consumption,
            TokenConsumption {
                input_tokens: 130,
                cached_input_tokens: 50,
                output_tokens: 30,
            }
        );
        assert_eq!(consumption.total_tokens().unwrap(), 160);
    }

    #[test]
    fn rejects_inconsistent_provider_usage() {
        let error = TokenConsumption::default()
            .observe_codex_jsonl(
                r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":11,"output_tokens":2}}"#,
            )
            .unwrap_err();
        assert!(error.to_string().contains("cached_input_tokens"));
    }
}
