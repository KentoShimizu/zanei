use zanei_core::store::PurgeFilter;

#[test]
fn universal_purge_filter_requires_an_unbounded_star_selection() {
    assert!(PurgeFilter::all().is_universal());
    assert!(
        PurgeFilter {
            types: vec!["content.*".to_owned(), "*".to_owned()],
            ..PurgeFilter::default()
        }
        .is_universal()
    );

    for filter in [
        PurgeFilter {
            types: vec!["content.*".to_owned()],
            ..PurgeFilter::default()
        },
        PurgeFilter::before_all("2026-08-24T00:00:00Z"),
        PurgeFilter {
            types: vec!["*".to_owned()],
            app: Some("Slack".to_owned()),
            ..PurgeFilter::default()
        },
    ] {
        assert!(!filter.is_universal(), "scoped filter: {filter:?}");
    }
}
