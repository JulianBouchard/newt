use super::InboundEvent;

#[test]
fn a_float_in_meta_is_stored_as_the_exact_number_the_client_sent() {
    let sent = -1.492_051_495_843_621e59_f64;
    let body = format!(r#"{{"id":"a","text":"b","meta":{{"k":{sent:e}}}}}"#);

    let event: InboundEvent = serde_json::from_str(&body).expect("a valid event");

    let stored = event.meta.and_then(|meta| meta["k"].as_f64());
    assert_eq!(stored, Some(sent));
}
