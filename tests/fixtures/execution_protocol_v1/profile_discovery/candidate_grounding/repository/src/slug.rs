pub fn normalize_slug(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(' ', "-")
}

