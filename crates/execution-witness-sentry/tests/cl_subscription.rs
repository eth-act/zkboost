use execution_witness_sentry::subscribe_cl_events;

#[test]
fn subscribe_cl_events_accepts_base_url_without_trailing_slash() {
    assert!(subscribe_cl_events("http://localhost:5052").is_ok());
}

#[test]
fn subscribe_cl_events_accepts_base_url_with_trailing_slash() {
    assert!(subscribe_cl_events("http://localhost:5052/").is_ok());
}
