use discovery_candidate_fixture::normalize_slug;

#[test]
fn normalizes_words_for_a_slug() {
    assert_eq!(normalize_slug("Example Title"), "example-title");
}

