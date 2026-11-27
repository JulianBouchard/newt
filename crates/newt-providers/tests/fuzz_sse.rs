#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use newt_providers::sse::{SseEvent, SseParser};
use proptest::prelude::*;

fn sse_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just("data: ".to_owned()),
            Just("data:".to_owned()),
            Just("event: ".to_owned()),
            Just("id: 1".to_owned()),
            Just(": comment".to_owned()),
            Just("\n".to_owned()),
            Just("\r\n".to_owned()),
            Just("\n\n".to_owned()),
            Just("\r\n\r\n".to_owned()),
            Just("{\"a\":\"\u{e9}\u{1f642}\"}".to_owned()),
            "[a-z \u{e9}\u{65e5}]{0,5}",
        ],
        0..30,
    )
    .prop_map(|pieces| pieces.concat())
}

fn feed_all(chunks: &[&[u8]]) -> Result<Vec<SseEvent>, String> {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.feed(chunk).map_err(|error| error.to_string())?);
    }
    Ok(events)
}

proptest! {
    #[test]
    fn chunking_never_changes_the_events(
        text in sse_text(),
        cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
    ) {
        let bytes = text.as_bytes();
        let mut points: Vec<usize> = cuts.iter().map(|cut| cut.index(bytes.len() + 1)).collect();
        points.sort_unstable();
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut start = 0;
        for point in points {
            chunks.push(&bytes[start..point]);
            start = point;
        }
        chunks.push(&bytes[start..]);
        prop_assert_eq!(feed_all(&chunks), feed_all(&[bytes]));
    }

    #[test]
    fn arbitrary_bytes_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        cut in any::<prop::sample::Index>(),
    ) {
        let at = cut.index(bytes.len() + 1);
        let _ = feed_all(&[&bytes[..at], &bytes[at..]]);
    }

    #[test]
    fn every_event_is_dispatched_by_its_blank_line(data in "[a-z\u{e9} ]{0,12}", count in 1usize..6) {
        let stream = format!("data: {data}\r\n\r\n").repeat(count);
        let events = feed_all(&[stream.as_bytes()]).unwrap();
        prop_assert_eq!(events.len(), count);
        prop_assert!(events.iter().all(|event| event.data == data));
    }
}
