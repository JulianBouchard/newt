use std::cell::RefCell;

use proptest::prelude::*;

use super::*;

#[derive(Debug, Clone)]
enum Step {
    Push(String),
    Flush,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let delta = prop_oneof![
        any::<String>(),
        "[a-z .!?\n]{0,6}",
        Just(String::new()),
        Just("\n".to_owned()),
    ];
    prop::collection::vec(
        prop_oneof![delta.prop_map(Step::Push), Just(Step::Flush)],
        0..40,
    )
}

fn pushed(steps: &[Step]) -> String {
    steps
        .iter()
        .filter_map(|step| match step {
            Step::Push(delta) => Some(delta.as_str()),
            Step::Flush => None,
        })
        .collect()
}

proptest! {
    #[test]
    fn text_keeps_every_byte_in_order(steps in steps()) {
        let sent = RefCell::new(Vec::new());
        let emit = |text: String| sent.borrow_mut().push(text);
        let mut coalescer = TextCoalescer::default();
        for step in &steps {
            match step {
                Step::Push(delta) => coalescer.push(delta, &emit),
                Step::Flush => coalescer.flush(&emit),
            }
        }
        coalescer.flush(&emit);
        let sent = sent.into_inner();
        prop_assert!(sent.iter().all(|piece| !piece.is_empty()));
        prop_assert_eq!(sent.concat(), pushed(&steps));
    }

    #[test]
    fn tool_inputs_keep_each_calls_bytes_apart_and_in_order(
        ops in prop::collection::vec((0usize..3, any::<String>(), any::<bool>()), 0..40),
    ) {
        let sent = RefCell::new(Vec::new());
        let emit = |id: &str, name: &str, text: String| {
            sent.borrow_mut().push((id.to_owned(), name.to_owned(), text));
        };
        let mut inputs = ToolInputs::default();
        let ids = ["a", "b", "c"];
        for id in &ids[..2] {
            inputs.start((*id).to_owned(), format!("tool_{id}"));
        }
        let mut expected = [String::new(), String::new(), String::new()];
        for (index, delta, flush) in &ops {
            inputs.push(ids[*index], delta, &emit);
            if *index < 2 {
                expected[*index].push_str(delta);
            }
            if *flush {
                inputs.flush(ids[*index], &emit);
            }
        }
        inputs.flush_all(&emit);
        let sent = sent.into_inner();
        for (index, id) in ids.iter().enumerate() {
            let got: String = sent
                .iter()
                .filter(|(call, name, _)| call == id && *name == format!("tool_{id}"))
                .map(|(_, _, text)| text.as_str())
                .collect();
            prop_assert_eq!(&got, &expected[index]);
        }
    }
}
